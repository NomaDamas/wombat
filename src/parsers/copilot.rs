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

/// Discover GitHub Copilot CLI sessions: `<copilot_home>/session-state/<session-id>/events.jsonl`.
///
/// A root may be `~/.copilot` (or `$COPILOT_HOME`), its `session-state/` dir, or one session dir.
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for events in event_files(root) {
        let Ok(version) = file_version_token(&events) else {
            continue;
        };
        out.push(SourceCandidate {
            agent: AgentKind::Copilot,
            item_key: build_file_item_key(AgentKind::Copilot, &events),
            version,
            source_path: events.to_string_lossy().into_owned(),
            fs_path: events,
            inner_id: None,
        });
    }
    Ok(out)
}

fn event_files(root: &Path) -> Vec<PathBuf> {
    let direct = root.join("events.jsonl");
    if direct.is_file() {
        return vec![direct];
    }
    let state = if root.join("session-state").is_dir() {
        root.join("session-state")
    } else {
        root.to_path_buf()
    };
    let Ok(entries) = std::fs::read_dir(&state) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("events.jsonl"))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
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
        let data = v.get("data").unwrap_or(&Value::Null);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "session.start" => {
                session_id = data
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                cwd = data
                    .get("context")
                    .and_then(|c| c.get("cwd"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            // `content` is exactly what the user typed; `transformedContent` is the prompt after
            // the CLI injected datetime / context blocks.
            "user.message" => push_text(&mut turns, Role::User, data, ts, line_no),
            // Assistant messages that only carry `toolRequests` have empty `content`; reasoning
            // lives in `reasoningText`/`reasoningOpaque`, which are never read.
            "assistant.message" => push_text(&mut turns, Role::Assistant, data, ts, line_no),
            _ => {}
        }
    }

    let ws = read_workspace(path);
    if cwd.is_none() {
        cwd = ws.cwd.clone();
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
    let resolved_id = session_id
        .or(ws.id)
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "copilot-session".to_string());
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Copilot,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at: ws.created_at.or(started_at),
        ended_at: ws.updated_at.or(ended_at),
        title: ws.name,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

fn push_text(turns: &mut Vec<Turn>, role: Role, data: &Value, ts: Option<String>, line_no: u64) {
    let text = data
        .get("content")
        .and_then(Value::as_str)
        .map(strip_injected_system_blocks)
        .unwrap_or_default();
    push_turn_dedup(
        turns,
        role,
        text,
        ts,
        SourceOffset::Lines {
            start_line: line_no,
            end_line: line_no,
        },
    );
}

#[derive(Default)]
struct Workspace {
    id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

/// `workspace.yaml` beside `events.jsonl` is flat `key: value` lines; the title is volatile
/// (catalog-only) and the timestamps override the event timestamps.
fn read_workspace(events: &Path) -> Workspace {
    let mut ws = Workspace::default();
    let Some(dir) = events.parent() else {
        return ws;
    };
    let Ok(raw) = std::fs::read_to_string(dir.join("workspace.yaml")) else {
        return ws;
    };
    for line in raw.lines() {
        let Some((k, v)) = line.split_once(": ") else {
            continue;
        };
        let v = v.trim().trim_matches('"').to_string();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "id" => ws.id = Some(v),
            "cwd" => ws.cwd = Some(v),
            "name" => ws.name = Some(v),
            "created_at" => ws.created_at = Some(v),
            "updated_at" => ws.updated_at = Some(v),
            _ => {}
        }
    }
    ws
}
