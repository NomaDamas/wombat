use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn, ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, file_version_token, open_jsonl_reader, open_live_sqlite,
    push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// First line of the prompt Aside injects into a forked worker (subagent) session.
const FORK_PROMPT_PREFIX: &str = "<system_message>\nSTOP. READ THIS FIRST.";

/// Discover Aside browser-agent sessions.
///
/// A root may be `~/.aside`, `~/.aside/u`, an account dir (`~/.aside/u/0`) or its `sessions/`
/// dir. Each session is `<account>/sessions/<YYYY-MM-DD>_<id>/messages.jsonl`; titles, parent
/// links and trigger types live in `<account>/state.db` (`sessions` table).
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for account in account_dirs(root) {
        let meta = load_meta(&account);
        let Ok(entries) = std::fs::read_dir(account.join("sessions")) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let log = dir.join("messages.jsonl");
            let Some(sid) = dir
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(session_id_from_dir_name)
            else {
                continue;
            };
            let Ok(file_ver) = file_version_token(&log) else {
                continue;
            };
            let updated = meta.get(&sid).map_or(0, |m| m.updated_at);
            out.push(SourceCandidate {
                agent: AgentKind::Aside,
                item_key: format!(
                    "aside/{}/{}",
                    short_sha256(&account.to_string_lossy()),
                    safe_component(&sid, 120)
                ),
                version: format!("{file_ver}:{updated}"),
                source_path: log.to_string_lossy().into_owned(),
                fs_path: log,
                inner_id: Some(sid),
            });
        }
    }
    Ok(out)
}

fn account_dirs(root: &Path) -> Vec<PathBuf> {
    let has_sessions = |p: &Path| p.join("sessions").is_dir();
    if root.file_name().and_then(|s| s.to_str()) == Some("sessions")
        && let Some(parent) = root.parent()
    {
        return vec![parent.to_path_buf()];
    }
    if has_sessions(root) {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    for base in [root.join("u"), root.to_path_buf()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| has_sessions(p))
            .collect();
        found.sort();
        if !found.is_empty() {
            out = found;
            break;
        }
    }
    out
}

fn session_id_from_dir_name(name: &str) -> Option<String> {
    // `2026-11-01_CQeyc8lBJBD2Jh0d`
    let (date, id) = name.split_once('_')?;
    let b = date.as_bytes();
    let date_like = b.len() == 10 && b[4] == b'-' && b[7] == b'-';
    (date_like && !id.is_empty()).then(|| id.to_string())
}

struct SessionMeta {
    parent_id: Option<String>,
    title: Option<String>,
    trigger_type: Option<String>,
    ephemeral: bool,
    created_at: i64,
    updated_at: i64,
}

fn load_meta(account: &Path) -> HashMap<String, SessionMeta> {
    let db_path = account.join("state.db");
    let mut out = HashMap::new();
    if !db_path.is_file() {
        return out;
    }
    let Ok(conn) = open_live_sqlite(&db_path) else {
        return out;
    };
    let _ = read_meta(&conn, &mut out);
    out
}

fn read_meta(conn: &Connection, out: &mut HashMap<String, SessionMeta>) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, title, trigger, COALESCE(ephemeral, 0),
                COALESCE(created_at, 0), COALESCE(updated_at, 0)
         FROM sessions",
    )?;
    let rows = stmt.query_map(params![], |r| {
        let trigger: Option<String> = r.get(3)?;
        Ok((
            r.get::<_, String>(0)?,
            SessionMeta {
                parent_id: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                title: r.get(2)?,
                trigger_type: trigger
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string)),
                ephemeral: r.get::<_, i64>(4)? != 0,
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
            },
        ))
    })?;
    for row in rows {
        let (id, meta) = row?;
        out.insert(id, meta);
    }
    Ok(())
}

pub fn parse_file(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(sid) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    let path = &candidate.fs_path;
    // <account>/sessions/<dir>/messages.jsonl
    let account = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut meta_map = load_meta(&account);
    let meta = meta_map.remove(sid);

    let reader = open_jsonl_reader(path)?;
    let mut turns: Vec<Turn> = Vec::new();
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    // After a routine/heartbeat trigger, assistant text answers the automation, not the user,
    // until the next real user message.
    let mut in_routine_turn = false;

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
        let ts_ms = v.get("timestamp").and_then(Value::as_i64);
        if let Some(t) = ts_ms {
            first_ts.get_or_insert(t);
            last_ts = Some(t);
        }
        let offset = SourceOffset::Lines {
            start_line: line_no,
            end_line: line_no,
        };
        let ts = ts_ms.and_then(ms_to_rfc3339);
        match v.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                in_routine_turn = false;
                push_turn_dedup(
                    &mut turns,
                    Role::User,
                    content_text(v.get("content")),
                    ts,
                    offset,
                );
            }
            "assistant" if !in_routine_turn => {
                push_turn_dedup(
                    &mut turns,
                    Role::Assistant,
                    content_text(v.get("content")),
                    ts,
                    offset,
                );
            }
            "system-message"
                if v.get("kind").and_then(Value::as_str) == Some("routine_trigger") =>
            {
                in_routine_turn = true;
            }
            // `steering` duplicates the `user` record written when the steer is delivered;
            // tool results, lifecycle, metadata and compaction summaries are not dialogue.
            _ => {}
        }
    }

    let is_subagent = match &meta {
        Some(m) => {
            m.trigger_type.as_deref() == Some("subagent")
                || m.parent_id.is_some() && m.trigger_type.as_deref() != Some("routine")
        }
        // Forks replay the parent's turns first; the directive arrives as a later user message.
        None => turns
            .iter()
            .any(|t| t.role == Role::User && t.text.starts_with(FORK_PROMPT_PREFIX)),
    };
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    let is_automation = meta.as_ref().is_some_and(|m| {
        m.ephemeral
            || matches!(
                m.trigger_type.as_deref(),
                Some("routine" | "standalone-routine")
            )
    });
    if is_automation && !opts.include_ephemeral {
        return Ok(ParseOutcome::Skipped {
            reason: "automation",
        });
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let started_at = meta
        .as_ref()
        .filter(|m| m.created_at > 0)
        .and_then(|m| ms_to_rfc3339(m.created_at * 1000))
        .or_else(|| first_ts.and_then(ms_to_rfc3339));
    let ended_at = meta
        .as_ref()
        .filter(|m| m.updated_at > 0)
        .and_then(|m| ms_to_rfc3339(m.updated_at * 1000))
        .or_else(|| last_ts.and_then(ms_to_rfc3339));
    let title = meta
        .as_ref()
        .and_then(|m| m.title.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "New Session")
        .map(str::to_string);
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Aside,
        session_id: sid.to_string(),
        source_path: candidate.source_path.clone(),
        cwd: None,
        started_at,
        ended_at,
        title,
        is_subagent,
        parent_session_id: meta.and_then(|m| m.parent_id),
        turns,
    }))
}

/// User-visible text of one message: plain string, or only the `text` blocks of a block list
/// (`thinking`, `toolCall` and images are dropped).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => strip_injected_system_blocks(s),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(strip_injected_system_blocks)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}
