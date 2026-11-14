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

/// Discover Droid (Factory) sessions: `<factory_home>/sessions/<encoded-cwd>/<session-id>.jsonl`.
///
/// A root may be `~/.factory` (or `$FACTORY_HOME`), its `sessions/` dir, or one project dir.
/// Sibling `<id>.settings.json` (and `.bak`) files are metadata, not sessions.
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for dir in project_dirs(root) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(version) = file_version_token(&path) else {
                continue;
            };
            out.push(SourceCandidate {
                agent: AgentKind::Droid,
                item_key: build_file_item_key(AgentKind::Droid, &path),
                version,
                source_path: path.to_string_lossy().into_owned(),
                fs_path: path,
                inner_id: None,
            });
        }
    }
    Ok(out)
}

fn has_sessions(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
    })
}

fn project_dirs(root: &Path) -> Vec<PathBuf> {
    if has_sessions(root) {
        return vec![root.to_path_buf()];
    }
    let sessions = if root.join("sessions").is_dir() {
        root.join("sessions")
    } else {
        root.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
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
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "session_start" => {
                session_id = v.get("id").and_then(Value::as_str).map(str::to_string);
                cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
                title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
            }
            "message" => {
                let ts = v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(t) = &ts {
                    started_at.get_or_insert_with(|| t.clone());
                    ended_at = Some(t.clone());
                }
                let Some(msg) = v.get("message") else {
                    continue;
                };
                let role = match msg.get("role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                // Droid stores harness-written messages with the *user* role; they are marked by
                // their ids (`context-…` tool catalog, `__internal__…` reminders).
                if role == Role::User && is_injected_id(id) {
                    continue;
                }
                let text = content_text(msg.get("content"));
                if role == Role::User && is_injected_text(&text) {
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
            _ => {}
        }
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
            .unwrap_or("droid-session")
            .to_string()
    });
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Droid,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

fn is_injected_id(id: &str) -> bool {
    id.starts_with("context-") || id.starts_with("__internal__")
}

/// Harness notices Droid sends as user messages without a recognizable id.
fn is_injected_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<system-reminder>") && t.trim_end().ends_with("</system-reminder>")
        || t.starts_with("Custom model \"") && t.contains("does not map to a built-in model")
}

/// Visible text: a plain string, or only `text` parts (`thinking`, `tool_use`, `tool_result` dropped).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => strip_injected_system_blocks(s),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(strip_injected_system_blocks)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}
