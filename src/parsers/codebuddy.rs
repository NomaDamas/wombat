use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn, is_ephemeral_temp_cwd,
    ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, build_file_item_key, file_version_token, open_jsonl_reader,
    push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::Result;
use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Discover CodeBuddy Code sessions: `<config_dir>/projects/<encoded-cwd>/<session-id>.jsonl`.
///
/// A root may be `~/.codebuddy` (or `$CODEBUDDY_CONFIG_DIR`) or its `projects/` dir.
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let projects = if root.join("projects").is_dir() {
        root.join("projects")
    } else {
        root.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Ok(Vec::new());
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = files.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(version) = file_version_token(&path) else {
                continue;
            };
            out.push(SourceCandidate {
                agent: AgentKind::Codebuddy,
                item_key: build_file_item_key(AgentKind::Codebuddy, &path),
                version,
                source_path: path.to_string_lossy().into_owned(),
                fs_path: path,
                inner_id: None,
            });
        }
    }
    Ok(out)
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
        if session_id.is_none() {
            session_id = v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        let ts = v.get("timestamp").and_then(timestamp_to_rfc3339);
        if let Some(t) = &ts {
            started_at.get_or_insert_with(|| t.clone());
            ended_at = Some(t.clone());
        }
        // Only `message` records are dialogue. function_call / function_call_result /
        // file-history-snapshot / session-meta / summary / turn-metrics are skipped.
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let role = match v.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        push_turn_dedup(
            &mut turns,
            role,
            content_text(v.get("content")),
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
            .unwrap_or("codebuddy-session")
            .to_string()
    });
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Codebuddy,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title: None,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

/// `timestamp` is epoch milliseconds, stored as a string (or a number).
fn timestamp_to_rfc3339(v: &Value) -> Option<String> {
    let ms = v
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| v.as_i64())?;
    ms_to_rfc3339(ms)
}

/// Visible text: a plain string, or only `input_text` / `output_text` / `text` parts
/// (`reasoning`, images and other part types are dropped).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => strip_injected_system_blocks(s),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| {
                matches!(
                    p.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
            })
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(strip_injected_system_blocks)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}
