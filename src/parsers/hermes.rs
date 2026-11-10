use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
    ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, file_version_token, open_readonly_sqlite, push_turn_dedup,
    strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use chrono::{LocalResult, NaiveDateTime, TimeZone};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Discover Hermes Agent sessions under a Hermes home (`~/.hermes`, or `$HERMES_HOME`).
///
/// A root may be the home directory or a `state.db` file. Sources, in priority order:
/// 1. `<home>/state.db` and `<home>/profiles/*/state.db` (`sessions` + `messages` tables).
/// 2. `<home>/sessions/session_<id>.json` and the same under profiles, for sessions that never
///    reached any DB (legacy snapshot files). `<id>.jsonl` transcripts are always DB-backed
///    mirrors of DB sessions and are ignored.
///
/// A session id is emitted once; the first home that carries it wins (profiles copy rows).
/// Cron-run sessions (`source = 'cron'`) are machine-issued and skipped unless `include_ephemeral`.
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let homes = hermes_homes(root);
    let root_hash = short_sha256(&root.to_string_lossy());
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();

    for home in &homes {
        let db_path = if root.is_file() {
            root.to_path_buf()
        } else {
            home.join("state.db")
        };
        if db_path.is_file() {
            out.extend(discover_db(&db_path, &root_hash, opts, &mut seen)?);
        }
    }
    for home in &homes {
        discover_json_only(home, &root_hash, opts, &mut seen, &mut out);
    }
    Ok(out)
}

fn hermes_homes(root: &Path) -> Vec<PathBuf> {
    let home = if root.is_file() {
        root.parent().unwrap_or(root).to_path_buf()
    } else {
        root.to_path_buf()
    };
    let mut homes = vec![home.clone()];
    if root.is_dir()
        && let Ok(entries) = std::fs::read_dir(home.join("profiles"))
    {
        let mut profiles: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        profiles.sort();
        homes.extend(profiles);
    }
    homes
}

fn open_hermes_db(db_path: &Path) -> Result<rusqlite::Connection> {
    // Hermes keeps state.db in WAL mode and a gateway usually holds it open. A plain read-only
    // connection reads WAL content without copying the (hundreds of MB) database per session.
    if let Ok(conn) = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) && conn
        .busy_timeout(std::time::Duration::from_secs(5))
        .is_ok()
        && conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .is_ok()
    {
        return Ok(conn);
    }
    Ok(open_readonly_sqlite(db_path)?.conn)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    rows.flatten().any(|name| name == column)
}

fn discover_db(
    db_path: &Path,
    root_hash: &str,
    opts: &ParseOptions,
    seen: &mut HashSet<String>,
) -> Result<Vec<SourceCandidate>> {
    let conn = open_hermes_db(db_path)?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, COALESCE(s.source, ''), COUNT(m.id), COALESCE(MAX(m.id), 0),
                    COALESCE(MAX(m.timestamp), 0.0)
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id ORDER BY s.id",
        )
        .with_context(|| format!("{} is not a Hermes state.db", db_path.display()))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (sid, source, msg_count, max_id, max_ts) = row?;
        if !seen.insert(sid.clone()) {
            continue;
        }
        if msg_count == 0 || (source == "cron" && !opts.include_ephemeral) {
            continue;
        }
        out.push(SourceCandidate {
            agent: AgentKind::Hermes,
            item_key: format!("hermes/{root_hash}/{}", safe_component(&sid, 120)),
            version: format!("hdb:{msg_count}:{max_id}:{}", (max_ts * 1000.0) as i64),
            source_path: format!("{}#{sid}", db_path.display()),
            fs_path: db_path.to_path_buf(),
            inner_id: Some(sid),
        });
    }
    Ok(out)
}

fn discover_json_only(
    home: &Path,
    root_hash: &str,
    opts: &ParseOptions,
    seen: &mut HashSet<String>,
    out: &mut Vec<SourceCandidate>,
) {
    let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(sid) = name
            .strip_prefix("session_")
            .and_then(|s| s.strip_suffix(".json"))
        else {
            continue;
        };
        if !seen.insert(sid.to_string()) {
            continue;
        }
        if !opts.include_ephemeral && sid.starts_with("cron_") {
            continue;
        }
        let Ok(version) = file_version_token(&path) else {
            continue;
        };
        out.push(SourceCandidate {
            agent: AgentKind::Hermes,
            item_key: format!("hermes/{root_hash}/{}", safe_component(sid, 120)),
            version,
            source_path: path.to_string_lossy().into_owned(),
            fs_path: path.clone(),
            inner_id: Some(sid.to_string()),
        });
    }
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(sid) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    if candidate.fs_path.extension().and_then(|s| s.to_str()) == Some("json") {
        parse_json_session(&candidate.fs_path, sid, opts)
    } else {
        parse_db_session(&candidate.fs_path, sid, opts)
    }
}

/// Dialogue text of one Hermes message, or `None` if it is not user/assistant dialogue.
fn dialogue_text(role: &str, content: &str) -> Option<(Role, String)> {
    let role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let trimmed = content.trim();
    match role {
        // Harness-injected notices, not typed by the user.
        Role::User if trimmed.starts_with("[System note:") => return None,
        // Context-compaction handoff summaries and silent-delivery markers are harness output.
        Role::Assistant
            if trimmed.starts_with("[CONTEXT COMPACTION") || trimmed == "[SILENT]" =>
        {
            return None;
        }
        _ => {}
    }
    let cleaned = strip_injected_system_blocks(content);
    (!cleaned.is_empty()).then_some((role, cleaned))
}

fn parse_db_session(db_path: &Path, sid: &str, opts: &ParseOptions) -> Result<ParseOutcome> {
    let conn = open_hermes_db(db_path)?;
    let has_cwd = has_column(&conn, "sessions", "cwd");
    let has_parent = has_column(&conn, "sessions", "parent_session_id");
    let sql = format!(
        "SELECT COALESCE(source, ''), {}, started_at, ended_at, title, {} FROM sessions WHERE id = ?1",
        if has_parent { "parent_session_id" } else { "NULL" },
        if has_cwd { "cwd" } else { "NULL" },
    );
    type Meta = (
        String,
        Option<String>,
        Option<f64>,
        Option<f64>,
        Option<String>,
        Option<String>,
    );
    let meta: Option<Meta> = conn
        .query_row(&sql, params![sid], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .ok();
    let Some((source, parent, started, ended, title, cwd)) = meta else {
        return Ok(ParseOutcome::Skipped {
            reason: "session_missing",
        });
    };
    if !opts.include_ephemeral {
        if source == "cron" {
            return Ok(ParseOutcome::Skipped {
                reason: "cron_automation",
            });
        }
        if let Some(c) = cwd.as_deref()
            && is_ephemeral_temp_cwd(c)
        {
            return Ok(ParseOutcome::Skipped {
                reason: "ephemeral_cwd",
            });
        }
    }

    let mut filters = String::new();
    if has_column(&conn, "messages", "active") {
        filters.push_str(" AND active != 0");
    }
    if has_column(&conn, "messages", "display_kind") {
        filters.push_str(" AND COALESCE(display_kind, '') NOT IN ('failed_turn', 'hidden')");
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT id, role, content, timestamp FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant'){filters} ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![sid], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<f64>>(3)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (id, role, content, ts) = row?;
        let Some((role, text)) = dialogue_text(&role, content.as_deref().unwrap_or("")) else {
            continue;
        };
        push_turn_dedup(
            &mut turns,
            role,
            text,
            ts.and_then(secs_to_rfc3339),
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
        agent: AgentKind::Hermes,
        session_id: sid.to_string(),
        source_path: format!("{}#{sid}", db_path.display()),
        cwd: cwd.filter(|c| !c.trim().is_empty()),
        started_at: started.and_then(secs_to_rfc3339),
        ended_at: ended.and_then(secs_to_rfc3339),
        title: title.filter(|t| !t.trim().is_empty()),
        is_subagent: false,
        parent_session_id: parent.filter(|p| !p.is_empty()),
        turns,
    }))
}

fn parse_json_session(path: &Path, sid: &str, opts: &ParseOptions) -> Result<ParseOutcome> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid hermes session json {}", path.display()))?;
    if !opts.include_ephemeral && v.get("platform").and_then(Value::as_str) == Some("cron") {
        return Ok(ParseOutcome::Skipped {
            reason: "cron_automation",
        });
    }
    let mut turns = Vec::new();
    let empty = Vec::new();
    for (idx, msg) in v
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .enumerate()
    {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let Some((role, text)) = dialogue_text(role, &json_content_text(msg.get("content")))
        else {
            continue;
        };
        push_turn_dedup(
            &mut turns,
            role,
            text,
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
    let ts = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .and_then(naive_local_to_rfc3339)
    };
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Hermes,
        session_id: sid.to_string(),
        source_path: path.to_string_lossy().into_owned(),
        cwd: None,
        started_at: ts("session_start"),
        ended_at: ts("last_updated"),
        title: None,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

fn json_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                b.as_str().map(str::to_string).or_else(|| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str).map(str::to_string))
                        .flatten()
                })
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn secs_to_rfc3339(secs: f64) -> Option<String> {
    ms_to_rfc3339((secs * 1000.0) as i64)
}

/// Hermes writes naive local-time ISO strings (`2026-05-02T21:03:59.057973`) in snapshot files.
fn naive_local_to_rfc3339(raw: &str) -> Option<String> {
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    match chrono::Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => {
            ms_to_rfc3339(dt.timestamp_millis())
        }
        LocalResult::None => None,
    }
}

/// Raw records behind one dialogue turn: the turn's own row plus, for an assistant turn, the
/// tool calls/results since the previous dialogue message.
pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    if let Some((db_str, sid)) = source_path.rsplit_once('#')
        && Path::new(db_str).is_file()
    {
        let id: i64 = item_id.parse().context("hermes item id is not a number")?;
        let conn = open_hermes_db(Path::new(db_str))?;
        let role: String = conn.query_row(
            "SELECT role FROM messages WHERE session_id = ?1 AND id = ?2",
            params![sid, id],
            |r| r.get(0),
        )?;
        let lower: i64 = if role == "assistant" {
            conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM messages
                 WHERE session_id = ?1 AND id < ?2 AND role IN ('user', 'assistant')
                   AND COALESCE(content, '') != ''",
                params![sid, id],
                |r| r.get(0),
            )?
        } else {
            id - 1
        };
        let mut stmt = conn.prepare(
            "SELECT id, role, content, tool_calls, tool_name, tool_call_id, timestamp
             FROM messages WHERE session_id = ?1 AND id > ?2 AND id <= ?3 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![sid, lower, id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "role": r.get::<_, String>(1)?,
                "content": r.get::<_, Option<String>>(2)?,
                "tool_calls": r.get::<_, Option<String>>(3)?,
                "tool_name": r.get::<_, Option<String>>(4)?,
                "tool_call_id": r.get::<_, Option<String>>(5)?,
                "timestamp": r.get::<_, Option<f64>>(6)?,
            }))
        })?;
        let records: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
        return Ok(serde_json::to_string_pretty(&records)?);
    }

    let raw = std::fs::read_to_string(source_path)
        .with_context(|| format!("failed reading {source_path}"))?;
    let v: Value = serde_json::from_str(&raw)?;
    let idx: usize = item_id.parse().context("hermes item id is not a number")?;
    let msgs = v
        .get("messages")
        .and_then(Value::as_array)
        .context("hermes session json has no messages")?;
    let is_dialogue = |m: &Value| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("user" | "assistant")
        ) && !json_content_text(m.get("content")).trim().is_empty()
    };
    let is_assistant = msgs
        .get(idx)
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    let lower = if is_assistant {
        (0..idx)
            .rev()
            .find(|i| is_dialogue(&msgs[*i]))
            .map_or(0, |i| i + 1)
    } else {
        idx
    };
    let hi = (idx + 1).min(msgs.len());
    Ok(serde_json::to_string_pretty(&msgs[lower.min(hi)..hi])?)
}
