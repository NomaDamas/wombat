use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
    ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, open_readonly_sqlite, push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Discover OpenCode sessions from:
/// 1. `<root>/opencode*.db` SQLite database (`session_v2`/`session_message` or `session`/`message`/`part`), or a direct `.db` path.
/// 2. Legacy JSON file tree `<root>/storage/session/<project>/<sid>.json` (or `<root>/session/...`).
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let db_files = find_opencode_dbs(root);
    if !db_files.is_empty() {
        let mut out = Vec::new();
        for db_path in db_files {
            out.extend(discover_from_sqlite(&db_path, opts)?);
        }
        return Ok(out);
    }

    let storage_root = if root.join("storage").is_dir() {
        root.join("storage")
    } else {
        root.to_path_buf()
    };
    discover_from_json_storage(&storage_root, opts)
}

fn find_opencode_dbs(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dbs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|s| s.to_str()).is_some_and(|n| {
                    let low = n.to_ascii_lowercase();
                    low.starts_with("opencode") && low.ends_with(".db")
                })
        })
        .collect();
    dbs.sort();
    dbs
}

fn discover_from_sqlite(db_path: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let db = open_readonly_sqlite(db_path)?;
    let conn = &db.conn;
    let has_v2 = has_table(conn, "session_v2");
    let has_v1 = has_table(conn, "session");
    let mut rows_meta: Vec<(String, String, i64, i64)> = Vec::new();

    if has_v2 {
        let mut stmt = conn.prepare(
            "SELECT s.id, COALESCE(s.parent_id, ''), COALESCE(s.time_updated, s.time_created, 0),
                    COUNT(m.id)
             FROM session_v2 s
             LEFT JOIN session_message m ON m.session_id = s.id
             GROUP BY s.id",
        )?;
        for r in stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })? {
            rows_meta.push(r?);
        }
    }
    if has_v1 && (!has_v2 || !is_v2_migration_done(conn)) {
        let sql = if has_v2 {
            "SELECT s.id, COALESCE(s.parent_id, ''), COALESCE(s.time_updated, s.time_created, 0),
                    COUNT(m.id), COALESCE(MAX(m.time_updated), 0)
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             WHERE s.id NOT IN (SELECT id FROM session_v2)
             GROUP BY s.id"
        } else {
            "SELECT s.id, COALESCE(s.parent_id, ''), COALESCE(s.time_updated, s.time_created, 0),
                    COUNT(m.id), COALESCE(MAX(m.time_updated), 0)
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             GROUP BY s.id"
        };
        let mut stmt = conn.prepare(sql)?;
        for r in stmt.query_map([], |row| {
            let s_upd: i64 = row.get(2)?;
            let m_upd: i64 = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                s_upd.max(m_upd),
                row.get::<_, i64>(3)?,
            ))
        })? {
            rows_meta.push(r?);
        }
    }

    let db_hash = short_sha256(&db_path.to_string_lossy());
    let mut out = Vec::new();
    for (sid, parent_id, updated_ms, msg_count) in rows_meta {
        if !opts.include_subagents && !parent_id.is_empty() {
            continue;
        }
        out.push(SourceCandidate {
            agent: AgentKind::OpenCode,
            item_key: format!("opencode/{db_hash}/{}", safe_component(&sid, 120)),
            version: format!("ocdb:{updated_ms}:{msg_count}"),
            source_path: format!("{}#{sid}", db_path.display()),
            fs_path: db_path.to_path_buf(),
            inner_id: Some(sid),
        });
    }
    Ok(out)
}

fn discover_from_json_storage(
    storage_root: &Path,
    opts: &ParseOptions,
) -> Result<Vec<SourceCandidate>> {
    let session_root = storage_root.join("session");
    if !session_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let Ok(proj_dirs) = std::fs::read_dir(&session_root) else {
        return Ok(out);
    };
    for proj in proj_dirs.flatten() {
        let ppath = proj.path();
        if !ppath.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&ppath) else {
            continue;
        };
        for f in files.flatten() {
            let fpath = f.path();
            if fpath.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&fpath) else {
                continue;
            };
            let Ok(info) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(sid) = info.get("id").and_then(Value::as_str) else {
                continue;
            };
            let parent_id = info.get("parentID").and_then(Value::as_str).unwrap_or("");
            if !opts.include_subagents && !parent_id.is_empty() {
                continue;
            }
            // Compute composite version over message + part directories for this session.
            let (msg_bytes, max_mtime) = composite_json_session_stamp(storage_root, sid, &fpath);
            let root_hash = short_sha256(&storage_root.to_string_lossy());
            out.push(SourceCandidate {
                agent: AgentKind::OpenCode,
                item_key: format!("opencode/{root_hash}/{}", safe_component(sid, 120)),
                version: format!("ocjson:{msg_bytes}:{max_mtime}"),
                source_path: fpath.to_string_lossy().into_owned(),
                fs_path: storage_root.to_path_buf(),
                inner_id: Some(sid.to_string()),
            });
        }
    }
    Ok(out)
}

fn composite_json_session_stamp(
    storage_root: &Path,
    sid: &str,
    session_file: &Path,
) -> (u64, u128) {
    let mut total_size = 0u64;
    let mut max_mtime = file_mtime_ns(session_file);
    let msg_dir = storage_root.join("message").join(sid);
    if let Ok(entries) = std::fs::read_dir(msg_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                total_size += meta.len();
                max_mtime = max_mtime.max(meta_mtime_ns(&meta));
            }
            if let Some(mid) = p.file_stem().and_then(|s| s.to_str()) {
                let part_dir = storage_root.join("part").join(mid);
                if let Ok(parts) = std::fs::read_dir(part_dir) {
                    for pe in parts.flatten() {
                        if let Ok(pmeta) = pe.metadata()
                            && pmeta.is_file()
                        {
                            total_size += pmeta.len();
                            max_mtime = max_mtime.max(meta_mtime_ns(&pmeta));
                        }
                    }
                }
            }
        }
    }
    (total_size, max_mtime)
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(sid) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    if candidate.fs_path.is_file() {
        parse_sqlite_session(&candidate.fs_path, sid, opts)
    } else {
        parse_json_storage_session(&candidate.fs_path, &candidate.source_path, sid, opts)
    }
}

fn parse_sqlite_session(db_path: &Path, sid: &str, opts: &ParseOptions) -> Result<ParseOutcome> {
    let db = open_readonly_sqlite(db_path)?;
    let conn = &db.conn;
    if has_table(conn, "session_v2")
        && conn
            .query_row(
                "SELECT 1 FROM session_v2 WHERE id = ?1",
                params![sid],
                |_| Ok(()),
            )
            .is_ok()
    {
        return parse_sqlite_v2(conn, db_path, sid, opts);
    }
    parse_sqlite_v1(conn, db_path, sid, opts)
}

fn parse_sqlite_v2(
    conn: &Connection,
    db_path: &Path,
    sid: &str,
    opts: &ParseOptions,
) -> Result<ParseOutcome> {
    let (parent_id, directory, title_raw, created_ms, updated_ms): (
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT COALESCE(parent_id, ''), COALESCE(directory, ''), COALESCE(title, ''),
                time_created, time_updated
         FROM session_v2 WHERE id = ?1",
        params![sid],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;

    let is_subagent = !parent_id.is_empty();
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if !opts.include_ephemeral && !directory.is_empty() && is_ephemeral_temp_cwd(&directory) {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut stmt = conn.prepare(
        "SELECT id, type, data, time_created FROM session_message
         WHERE session_id = ?1 AND type IN ('user', 'assistant')
         ORDER BY seq ASC",
    )?;
    let mut turns = Vec::new();
    for row in stmt.query_map(params![sid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })? {
        let (mid, mtype, raw_data, row_ts) = row?;
        let Ok(data) = serde_json::from_str::<Value>(&raw_data) else {
            continue;
        };
        let ts_ms = data
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .or(row_ts);
        let ts = ts_ms.and_then(ms_to_rfc3339);
        match mtype.as_str() {
            "user" => {
                if let Some(txt) = data.get("text").and_then(Value::as_str) {
                    let cleaned = strip_injected_system_blocks(txt);
                    push_turn_dedup(
                        &mut turns,
                        Role::User,
                        cleaned,
                        ts,
                        SourceOffset::Item { item_id: mid },
                    );
                }
            }
            "assistant" => {
                let mut texts = Vec::new();
                if let Some(arr) = data.get("content").and_then(Value::as_array) {
                    for part in arr {
                        // Keep ONLY `text`; skip `reasoning`, `tool`, etc.
                        if part.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(t) = part.get("text").and_then(Value::as_str)
                        {
                            let cleaned = strip_injected_system_blocks(t);
                            if !cleaned.is_empty() {
                                texts.push(cleaned);
                            }
                        }
                    }
                }
                push_turn_dedup(
                    &mut turns,
                    Role::Assistant,
                    texts.join("\n\n"),
                    ts,
                    SourceOffset::Item { item_id: mid },
                );
            }
            _ => {}
        }
    }

    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::OpenCode,
        session_id: sid.to_string(),
        source_path: format!("{}#{sid}", db_path.display()),
        cwd: (!directory.is_empty()).then_some(directory),
        started_at: created_ms.and_then(ms_to_rfc3339),
        ended_at: updated_ms.or(created_ms).and_then(ms_to_rfc3339),
        title: clean_oc_title(&title_raw),
        is_subagent,
        parent_session_id: (!parent_id.is_empty()).then_some(parent_id),
        turns,
    }))
}

fn parse_sqlite_v1(
    conn: &Connection,
    db_path: &Path,
    sid: &str,
    opts: &ParseOptions,
) -> Result<ParseOutcome> {
    let (parent_id, directory, title_raw, created_ms, updated_ms): (
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT COALESCE(parent_id, ''), COALESCE(directory, ''), COALESCE(title, ''),
                time_created, time_updated
         FROM session WHERE id = ?1",
        params![sid],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;

    let is_subagent = !parent_id.is_empty();
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if !opts.include_ephemeral && !directory.is_empty() && is_ephemeral_temp_cwd(&directory) {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut parts_by_msg: HashMap<String, Vec<Value>> = HashMap::new();
    {
        let mut pstmt = conn.prepare(
            "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY rowid ASC",
        )?;
        for r in pstmt.query_map(params![sid], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })? {
            let (mid, raw) = r?;
            if let Some(raw) = raw
                && let Ok(val) = serde_json::from_str::<Value>(&raw)
            {
                parts_by_msg.entry(mid).or_default().push(val);
            }
        }
    }

    let mut mstmt = conn.prepare(
        "SELECT id, time_created, data FROM message WHERE session_id = ?1 ORDER BY rowid ASC",
    )?;
    let mut turns = Vec::new();
    let mut fallback_cwd: Option<String> = None;
    for r in mstmt.query_map(params![sid], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (mid, row_created, raw_msg) = r?;
        let Some(raw_msg) = raw_msg else {
            continue;
        };
        let Ok(mval) = serde_json::from_str::<Value>(&raw_msg) else {
            continue;
        };
        let role_str = mval.get("role").and_then(Value::as_str).unwrap_or("");
        let role = match role_str {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if fallback_cwd.is_none()
            && let Some(c) = mval.pointer("/path/cwd").and_then(Value::as_str)
            && !c.is_empty()
        {
            fallback_cwd = Some(c.to_string());
        }
        let ts_ms = mval
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .or(row_created);
        let ts = ts_ms.and_then(ms_to_rfc3339);

        let text = collect_oc_text_parts(
            parts_by_msg.get(&mid).map(Vec::as_slice).unwrap_or(&[]),
            role,
        );
        push_turn_dedup(
            &mut turns,
            role,
            text,
            ts,
            SourceOffset::Item { item_id: mid },
        );
    }

    let final_cwd = if !directory.is_empty() {
        Some(directory)
    } else {
        fallback_cwd
    };
    if !opts.include_ephemeral
        && let Some(c) = final_cwd.as_deref()
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

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::OpenCode,
        session_id: sid.to_string(),
        source_path: format!("{}#{sid}", db_path.display()),
        cwd: final_cwd,
        started_at: created_ms.and_then(ms_to_rfc3339),
        ended_at: updated_ms.or(created_ms).and_then(ms_to_rfc3339),
        title: clean_oc_title(&title_raw),
        is_subagent,
        parent_session_id: (!parent_id.is_empty()).then_some(parent_id),
        turns,
    }))
}

fn parse_json_storage_session(
    storage_root: &Path,
    session_file_path: &str,
    sid: &str,
    opts: &ParseOptions,
) -> Result<ParseOutcome> {
    let raw = std::fs::read_to_string(session_file_path)
        .with_context(|| format!("failed reading {session_file_path}"))?;
    let info: Value = serde_json::from_str(&raw)?;
    let parent_id = info
        .get("parentID")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_subagent = !parent_id.is_empty();
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    let directory = info
        .get("directory")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !opts.include_ephemeral && !directory.is_empty() && is_ephemeral_temp_cwd(&directory) {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }
    let title_raw = info.get("title").and_then(Value::as_str).unwrap_or("");
    let created_ms = info.pointer("/time/created").and_then(Value::as_i64);
    let updated_ms = info.pointer("/time/updated").and_then(Value::as_i64);

    let msg_dir = storage_root.join("message").join(sid);
    let mut msgs: Vec<(i64, String, Value)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&msg_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(mraw) = std::fs::read_to_string(&p)
                && let Ok(mval) = serde_json::from_str::<Value>(&mraw)
                && let Some(mid) = mval.get("id").and_then(Value::as_str)
            {
                let created = mval
                    .pointer("/time/created")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                msgs.push((created, mid.to_string(), mval));
            }
        }
    }
    msgs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut turns = Vec::new();
    for (created, mid, mval) in msgs {
        let role = match mval.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let part_dir = storage_root.join("part").join(&mid);
        let mut part_files: Vec<PathBuf> = std::fs::read_dir(&part_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        part_files.sort();
        let mut pvals = Vec::new();
        for pf in part_files {
            if let Ok(praw) = std::fs::read_to_string(&pf)
                && let Ok(pv) = serde_json::from_str::<Value>(&praw)
            {
                pvals.push(pv);
            }
        }
        let text = collect_oc_text_parts(&pvals, role);
        push_turn_dedup(
            &mut turns,
            role,
            text,
            ms_to_rfc3339(created),
            SourceOffset::Item { item_id: mid },
        );
    }

    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::OpenCode,
        session_id: sid.to_string(),
        source_path: session_file_path.to_string(),
        cwd: (!directory.is_empty()).then_some(directory),
        started_at: created_ms.and_then(ms_to_rfc3339),
        ended_at: updated_ms.or(created_ms).and_then(ms_to_rfc3339),
        title: clean_oc_title(title_raw),
        is_subagent,
        parent_session_id: (!parent_id.is_empty()).then_some(parent_id),
        turns,
    }))
}

/// `opencode run "<prompt>"` stores the user's text part JSON-encoded (`"\"prompt\""`), so a
/// user text that is a complete JSON string literal is unwrapped. Assistant text is never touched.
fn unwrap_json_string_literal(text: &str) -> String {
    let t = text.trim();
    if t.len() >= 2
        && t.starts_with('"')
        && t.ends_with('"')
        && let Ok(inner) = serde_json::from_str::<String>(t)
    {
        return inner;
    }
    text.to_string()
}

fn collect_oc_text_parts(parts: &[Value], role: Role) -> String {
    let mut out = Vec::new();
    for p in parts {
        if p.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if p.get("ignored").and_then(Value::as_bool) == Some(true)
            || p.get("synthetic").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            let raw = if role == Role::User {
                unwrap_json_string_literal(t)
            } else {
                t.to_string()
            };
            let cleaned = strip_injected_system_blocks(&raw);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
    }
    out.join("\n\n")
}

fn clean_oc_title(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.starts_with("New session - ") || t.starts_with("Child session - ") {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    if let Some((db_str, sid)) = source_path.rsplit_once('#') {
        let db = open_readonly_sqlite(Path::new(db_str))?;
        let conn = &db.conn;
        if has_table(conn, "session_v2")
            && let Ok(data) = conn.query_row(
                "SELECT data FROM session_message WHERE session_id = ?1 AND id = ?2",
                params![sid, item_id],
                |r| r.get::<_, String>(0),
            )
        {
            return Ok(data);
        }
        if has_table(conn, "message") {
            return raw_v1_turn(conn, sid, item_id);
        }
    }
    // Legacy JSON storage: `.../storage/session/<proj>/<sid>.json`
    let session_path = Path::new(source_path);
    if let Some(storage_root) = session_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
    {
        let part_dir = storage_root.join("part").join(item_id);
        if part_dir.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&part_dir)?
                .flatten()
                .map(|e| e.path())
                .collect();
            entries.sort();
            let mut parts = Vec::new();
            for e in entries {
                if let Ok(s) = std::fs::read_to_string(e) {
                    parts.push(serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)));
                }
            }
            return Ok(serde_json::to_string_pretty(&parts)?);
        }
    }
    Ok(format!("source_path={source_path} item_id={item_id}"))
}

/// Raw records of one conversational turn. A user message maps to itself; an assistant message
/// maps to every message that answers the same user message (`parentID`), so the tool calls that
/// preceded the final text are part of the trace. Messages after `item_id` are not included.
fn raw_v1_turn(conn: &Connection, sid: &str, item_id: &str) -> Result<String> {
    let read_msg = |id: &str| -> Option<Value> {
        conn.query_row(
            "SELECT COALESCE(data, '') FROM message WHERE session_id = ?1 AND id = ?2",
            params![sid, id],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    };
    let own = read_msg(item_id);
    let parent = own
        .as_ref()
        .and_then(|m| m.get("parentID"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut ids: Vec<String> = Vec::new();
    if let Some(p) = &parent {
        ids.push(p.clone());
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(data, '') FROM message WHERE session_id = ?1 AND rowid <=
                (SELECT rowid FROM message WHERE session_id = ?1 AND id = ?2) ORDER BY rowid ASC",
        )?;
        for row in stmt.query_map(params![sid, item_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (id, data) = row?;
            let same_turn = serde_json::from_str::<Value>(&data)
                .ok()
                .and_then(|v| {
                    v.get("parentID")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .is_some_and(|pid| &pid == p);
            if same_turn {
                ids.push(id);
            }
        }
    } else {
        ids.push(item_id.to_string());
    }

    let mut out = Vec::new();
    for id in ids {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(data, '') FROM part WHERE session_id = ?1 AND message_id = ?2 ORDER BY rowid ASC",
        )?;
        let parts: Vec<Value> = stmt
            .query_map(params![sid, &id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|p| serde_json::from_str(&p).unwrap_or(Value::String(p)))
            .collect();
        out.push(serde_json::json!({
            "message_id": id,
            "message": read_msg(&id).unwrap_or(Value::Null),
            "parts": parts,
        }));
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

fn is_v2_migration_done(conn: &Connection) -> bool {
    if !has_table(conn, "kv") {
        return false;
    }
    let Ok(val) = conn.query_row(
        "SELECT value FROM kv WHERE key = 'migration.v1-v2'",
        [],
        |r| r.get::<_, String>(0),
    ) else {
        return false;
    };
    serde_json::from_str::<Value>(&val)
        .ok()
        .and_then(|v| {
            v.get("phase")
                .and_then(Value::as_str)
                .map(|p| p == "completed")
        })
        .unwrap_or(false)
}

fn file_mtime_ns(p: &Path) -> u128 {
    std::fs::metadata(p).map(|m| meta_mtime_ns(&m)).unwrap_or(0)
}

fn meta_mtime_ns(m: &std::fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
