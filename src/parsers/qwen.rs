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
use std::path::{Path, PathBuf};

/// Discover Qwen Code sessions: `<qwen_home>/projects/<encoded-cwd>/chats/<session-id>.jsonl`.
///
/// A root may be `~/.qwen`, its `projects/` dir, one project dir, or a `chats/` dir. Sibling
/// `<id>.runtime.json` / `<id>.title.json` files are not sessions.
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for chats in chats_dirs(root) {
        let Ok(entries) = std::fs::read_dir(&chats) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let is_log = path.extension().and_then(|s| s.to_str()) == Some("jsonl");
            if !is_log {
                continue;
            }
            let Ok(version) = file_version_token(&path) else {
                continue;
            };
            out.push(SourceCandidate {
                agent: AgentKind::Qwen,
                item_key: build_file_item_key(AgentKind::Qwen, &path),
                version,
                source_path: path.to_string_lossy().into_owned(),
                fs_path: path,
                inner_id: None,
            });
        }
    }
    Ok(out)
}

fn chats_dirs(root: &Path) -> Vec<PathBuf> {
    if root.file_name().and_then(|s| s.to_str()) == Some("chats") {
        return vec![root.to_path_buf()];
    }
    let project_chats = |project: &Path| -> Option<PathBuf> {
        let c = project.join("chats");
        c.is_dir().then_some(c)
    };
    if let Some(c) = project_chats(root) {
        return vec![c];
    }
    let projects = if root.join("projects").is_dir() {
        root.join("projects")
    } else {
        root.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| project_chats(&e.path()))
        .collect();
    dirs.sort();
    dirs
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut ended_at: Option<String> = None;
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
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(t) = &ts {
            started_at.get_or_insert_with(|| t.clone());
            ended_at = Some(t.clone());
        }
        if session_id.is_none() {
            session_id = v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
        }

        // Dialogue is decided by the record `type`, not `message.role`: harness snapshots
        // (`type: "system"`) and tool results (`type: "tool_result"`) also carry role "user".
        let role = match v.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let Some(parts) = v
            .get("message")
            .and_then(|m| m.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let text = parts
            .iter()
            // `thought: true` parts are model reasoning; functionCall/functionResponse have no text.
            .filter(|p| p.get("thought").and_then(Value::as_bool) != Some(true))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(strip_injected_system_blocks)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if role == Role::User && is_ignored_user_text(&text) {
            continue;
        }
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
            .unwrap_or("qwen-session")
            .to_string()
    });
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Qwen,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title: title_for(path),
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

/// Display name from the sibling `<id>.title.json` (volatile, catalog-only).
fn title_for(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let title_path = path.with_file_name(format!("{stem}.title.json"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(title_path).ok()?).ok()?;
    let t = v.get("displayName")?.as_str()?.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Slash commands (`/help`, `/model`) are CLI control input, not dialogue.
fn is_ignored_user_text(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.starts_with('/')
}
