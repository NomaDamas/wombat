use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
    ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, build_file_item_key, file_version_token, push_turn_dedup,
    strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Discover Continue CLI (`cn`) sessions: `<continue_home>/sessions/<session-id>.json`.
///
/// A root may be `~/.continue` (or `$CONTINUE_GLOBAL_DIR`) or its `sessions/` dir. The sibling
/// `sessions.json` is only an index (title, creation time) and is not a session.
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let dir = if root.join("sessions").is_dir() {
        root.join("sessions")
    } else {
        root.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let is_session = path.extension().and_then(|s| s.to_str()) == Some("json")
            && path.file_name().and_then(|s| s.to_str()) != Some("sessions.json");
        if !is_session {
            continue;
        }
        let Ok(version) = file_version_token(&path) else {
            continue;
        };
        out.push(SourceCandidate {
            agent: AgentKind::Continue,
            item_key: build_file_item_key(AgentKind::Continue, &path),
            version,
            source_path: path.to_string_lossy().into_owned(),
            fs_path: path,
            inner_id: None,
        });
    }
    Ok(out)
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid continue session json {}", path.display()))?;
    let cwd = v
        .get("workspaceDirectory")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut turns = Vec::new();
    let empty = Vec::new();
    for (idx, entry) in v
        .get("history")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .enumerate()
    {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let role = match msg.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            // system / tool / thinking entries are not dialogue.
            _ => continue,
        };
        push_turn_dedup(
            &mut turns,
            role,
            content_text(msg.get("content")),
            None,
            SourceOffset::Item {
                item_id: idx.to_string(),
            },
        );
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }
    let session_id = v
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "continue-session".to_string());
    let started_at = index_created_at(path, &session_id);
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Continue,
        session_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at: None,
        title,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

/// Creation time (ms epoch, stored as a string) from the sibling `sessions.json` index.
fn index_created_at(path: &Path, session_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path.parent()?.join("sessions.json")).ok()?;
    let Value::Array(items) = serde_json::from_str::<Value>(&raw).ok()? else {
        return None;
    };
    let item = items
        .iter()
        .find(|i| i.get("sessionId").and_then(Value::as_str) == Some(session_id))?;
    let ms = item.get("dateCreated").and_then(|d| {
        d.as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| d.as_i64())
    })?;
    ms_to_rfc3339(ms)
}

/// Visible text: a plain string, or only the `text` parts of a part list (images are dropped).
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

/// Raw history entries behind a turn: its own entry plus, for an assistant reply, the
/// tool-calling entries since the previous dialogue message.
pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    let raw = std::fs::read_to_string(source_path)
        .with_context(|| format!("failed reading {source_path}"))?;
    let v: Value = serde_json::from_str(&raw)?;
    let history = v
        .get("history")
        .and_then(Value::as_array)
        .context("continue session has no history")?;
    let idx: usize = item_id
        .parse()
        .context("continue item id is not a number")?;
    let is_dialogue = |h: &Value| {
        let m = h.get("message");
        matches!(
            m.and_then(|m| m.get("role")).and_then(Value::as_str),
            Some("user" | "assistant")
        ) && !content_text(m.and_then(|m| m.get("content")))
            .trim()
            .is_empty()
    };
    let is_assistant = history
        .get(idx)
        .and_then(|h| h.get("message"))
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    let lower = if is_assistant {
        (0..idx)
            .rev()
            .find(|i| is_dialogue(&history[*i]))
            .map_or(0, |i| i + 1)
    } else {
        idx
    };
    let hi = (idx + 1).min(history.len());
    Ok(serde_json::to_string_pretty(&history[lower.min(hi)..hi])?)
}
