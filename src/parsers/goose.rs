use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, open_live_sqlite, push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Discover Goose sessions in `<goose data dir>/sessions/sessions.db` (SQLite, WAL-safe).
///
/// A root may be the data dir (`~/.local/share/goose`, or `$GOOSE_PATH_ROOT/data`), its
/// `sessions/` dir, or the `sessions.db` file.
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let Some(db) = find_db(root) else {
        return Ok(Vec::new());
    };
    let conn = open_live_sqlite(&db)?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, COALESCE(s.session_type, 'user'), s.parent_session_id,
                    COUNT(m.id), COALESCE(MAX(m.id), 0), COALESCE(s.updated_at, '')
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id ORDER BY s.created_at",
        )
        .with_context(|| format!("{} is not a Goose sessions.db", db.display()))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let db_hash = short_sha256(&db.to_string_lossy());
    let mut out = Vec::new();
    for row in rows {
        let (sid, session_type, parent, count, max_id, updated) = row?;
        if count == 0 || session_type == "hidden" {
            continue;
        }
        let is_sub = is_subagent(&session_type, parent.as_deref());
        if is_sub && !opts.include_subagents {
            continue;
        }
        out.push(SourceCandidate {
            agent: AgentKind::Goose,
            item_key: format!("goose/{db_hash}/{}", safe_component(&sid, 120)),
            version: format!("gdb:{count}:{max_id}:{updated}"),
            source_path: format!("{}#{sid}", db.display()),
            fs_path: db.clone(),
            inner_id: Some(sid),
        });
    }
    Ok(out)
}

fn is_subagent(session_type: &str, parent: Option<&str>) -> bool {
    session_type == "sub_agent" || parent.is_some_and(|p| !p.is_empty())
}

fn find_db(root: &Path) -> Option<PathBuf> {
    if root.is_file() {
        return Some(root.to_path_buf());
    }
    [
        root.join("sessions.db"),
        root.join("sessions").join("sessions.db"),
        root.join("data").join("sessions").join("sessions.db"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

struct SessionRow {
    name: String,
    session_type: String,
    cwd: String,
    created: Option<String>,
    updated: Option<String>,
    parent: Option<String>,
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(sid) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    let conn = open_live_sqlite(&candidate.fs_path)?;
    let row: Option<SessionRow> = conn
        .query_row(
            "SELECT COALESCE(name, ''), COALESCE(session_type, 'user'), COALESCE(working_dir, ''),
                    created_at, updated_at, parent_session_id
             FROM sessions WHERE id = ?1",
            params![sid],
            |r| {
                Ok(SessionRow {
                    name: r.get(0)?,
                    session_type: r.get(1)?,
                    cwd: r.get(2)?,
                    created: r.get(3)?,
                    updated: r.get(4)?,
                    parent: r.get(5)?,
                })
            },
        )
        .ok();
    let Some(SessionRow {
        name,
        session_type,
        cwd,
        created,
        updated,
        parent,
    }) = row
    else {
        return Ok(ParseOutcome::Skipped {
            reason: "session_missing",
        });
    };
    if session_type == "hidden" {
        return Ok(ParseOutcome::Skipped { reason: "hidden" });
    }
    let sub = is_subagent(&session_type, parent.as_deref());
    if sub && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if !opts.include_ephemeral && !cwd.is_empty() && is_ephemeral_temp_cwd(&cwd) {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut stmt = conn.prepare(
        "SELECT id, role, content_json, created_timestamp, COALESCE(metadata_json, '{}')
         FROM messages WHERE session_id = ?1 AND role IN ('user', 'assistant') ORDER BY id",
    )?;
    let rows = stmt.query_map(params![sid], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (id, role, content, ts, metadata) = row?;
        let meta: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        // Goose injects `<turn-context>` as an extra, user-invisible user message.
        if meta.get("userVisible").and_then(Value::as_bool) == Some(false)
            || meta.get("turnContext").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let role = if role == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        push_turn_dedup(
            &mut turns,
            role,
            text_of(&content),
            crate::model::ms_to_rfc3339(ts * 1000),
            SourceOffset::Item {
                item_id: id.to_string(),
            },
        );
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Goose,
        session_id: sid.to_string(),
        source_path: candidate.source_path.clone(),
        cwd: (!cwd.is_empty()).then_some(cwd),
        started_at: created.as_deref().and_then(sqlite_ts),
        ended_at: updated.as_deref().and_then(sqlite_ts),
        title: Some(name).filter(|n| !n.trim().is_empty()),
        is_subagent: sub,
        parent_session_id: parent.filter(|p| !p.is_empty()),
        turns,
    }))
}

/// `2026-11-13 14:21:54` (UTC, as SQLite `CURRENT_TIMESTAMP`) -> RFC 3339.
fn sqlite_ts(raw: &str) -> Option<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").ok()?;
    crate::model::ms_to_rfc3339(naive.and_utc().timestamp_millis())
}

/// Visible text of a message: only `text` blocks (`thinking`, `toolRequest`, `toolResponse`,
/// images and other block types are dropped).
fn text_of(content_json: &str) -> String {
    let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(content_json) else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .map(strip_injected_system_blocks)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Raw rows behind a turn: its own row plus, for an assistant reply, the tool-request /
/// tool-response / invisible rows since the previous visible dialogue row.
pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    let (db_str, sid) = source_path
        .rsplit_once('#')
        .context("goose source path has no #<session id>")?;
    let conn: Connection = open_live_sqlite(Path::new(db_str))?;
    let id: i64 = item_id.parse().context("goose item id is not a number")?;
    let role: String = conn.query_row(
        "SELECT role FROM messages WHERE session_id = ?1 AND id = ?2",
        params![sid, id],
        |r| r.get(0),
    )?;
    let lower: i64 = if role == "assistant" {
        conn.query_row(
            "SELECT COALESCE(MAX(m.id), 0) FROM messages m
             WHERE m.session_id = ?1 AND m.id < ?2 AND m.role IN ('user', 'assistant')
               AND EXISTS (SELECT 1 FROM json_each(m.content_json) b
                           WHERE json_extract(b.value, '$.type') = 'text')
               AND COALESCE(json_extract(m.metadata_json, '$.turnContext'), 0) = 0",
            params![sid, id],
            |r| r.get(0),
        )?
    } else {
        id - 1
    };
    let mut stmt = conn.prepare(
        "SELECT id, role, content_json, created_timestamp, COALESCE(metadata_json, '{}')
         FROM messages WHERE session_id = ?1 AND id > ?2 AND id <= ?3 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![sid, lower, id], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?,
            "role": r.get::<_, String>(1)?,
            "content": serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
            "created_timestamp": r.get::<_, i64>(3)?,
            "metadata": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(Value::Null),
        }))
    })?;
    let records: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
    Ok(serde_json::to_string_pretty(&records)?)
}
