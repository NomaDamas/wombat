use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn, is_ephemeral_temp_cwd,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, build_file_item_key, file_version_token, open_jsonl_reader,
    push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::Result;
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;
use walkdir::WalkDir;

pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if !opts.include_subagents && path.components().any(|c| c.as_os_str() == "subagents") {
            continue;
        }
        let Ok(version) = file_version_token(path) else {
            continue;
        };
        out.push(SourceCandidate {
            agent: AgentKind::ClaudeCode,
            item_key: build_file_item_key(AgentKind::ClaudeCode, path),
            version,
            source_path: path.to_string_lossy().into_owned(),
            fs_path: path.to_path_buf(),
            inner_id: None,
        });
    }
    Ok(out)
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    if !opts.include_subagents && path.components().any(|c| c.as_os_str() == "subagents") {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }

    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut ended_at: Option<String> = None;
    let mut title: Option<String> = None;
    let mut saw_non_sidechain = false;
    let mut saw_sidechain = false;
    let mut turns: Vec<Turn> = Vec::new();

    for (idx, line_res) in reader.lines().enumerate() {
        let line_no = (idx + 1) as u64;
        let Ok(line) = line_res else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if let Some(sid) = v.get("sessionId").and_then(Value::as_str)
            && session_id.is_none()
        {
            session_id = Some(sid.to_string());
        }
        if let Some(c) = v.get("cwd").and_then(Value::as_str)
            && cwd.is_none()
        {
            cwd = Some(c.to_string());
        }
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if let Some(t) = &ts {
            if started_at.is_none() {
                started_at = Some(t.clone());
            }
            ended_at = Some(t.clone());
        }

        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        match record_type {
            "ai-title" | "custom-title" => {
                if let Some(t) = v
                    .get("aiTitle")
                    .or_else(|| v.get("customTitle"))
                    .and_then(Value::as_str)
                {
                    title = Some(t.to_string());
                }
                continue;
            }
            "summary" => {
                if title.is_none()
                    && let Some(s) = v.get("summary").and_then(Value::as_str)
                {
                    title = Some(s.to_string());
                }
                continue;
            }
            "user" | "assistant" => {}
            // Ignore file-history-snapshot, attachment, system, queue-operation, pr-link, etc.
            _ => continue,
        }

        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let is_sidechain = v
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_sidechain {
            saw_sidechain = true;
            if !opts.include_subagents {
                continue;
            }
        } else {
            saw_non_sidechain = true;
        }

        let Some(msg) = v.get("message") else {
            continue;
        };
        let role = match record_type {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let text = extract_claude_content(msg.get("content"), role);
        push_turn_dedup(
            &mut turns,
            role,
            text,
            ts,
            SourceOffset::Lines {
                start_line: line_no,
                end_line: line_no,
            },
        );
    }

    let is_subagent = saw_sidechain && !saw_non_sidechain;
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let resolved_id = session_id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("claude-session")
            .to_string()
    });

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::ClaudeCode,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title,
        is_subagent,
        parent_session_id: None,
        turns,
    }))
}

fn extract_claude_content(content: Option<&Value>, role: Role) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return clean_claude_text(s, role);
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in arr {
        // Keep ONLY `text` blocks — strip `thinking`, `redacted_thinking`, `tool_use`, `tool_result`, `image`, `document`.
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(raw) = block.get("text").and_then(Value::as_str) {
            let cleaned = clean_claude_text(raw, role);
            if !cleaned.is_empty() {
                parts.push(cleaned);
            }
        }
    }
    parts.join("\n\n")
}

fn clean_claude_text(raw: &str, role: Role) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if role == Role::User {
        // Drop local slash commands, command stdout, interrupted markers, task notifications.
        if trimmed.starts_with("<command-name>")
            || trimmed.starts_with("<local-command-")
            || trimmed.starts_with("<task-notification>")
            || trimmed.starts_with("<bash-input>")
            || trimmed.starts_with("<bash-stdout>")
            || trimmed.starts_with("<bash-stderr>")
            || trimmed == "[Request interrupted by user]"
            || trimmed == "[Request interrupted by user for tool use]"
        {
            return String::new();
        }
    }
    strip_injected_system_blocks(trimmed)
}
