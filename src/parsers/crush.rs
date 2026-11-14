use crate::mirror::{safe_component, short_sha256};
use crate::model::ms_to_rfc3339;
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, open_live_sqlite, push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Discover Crush (Charm) sessions.
///
/// Crush keeps one SQLite DB per project (`<project>/.crush/crush.db`) and a shared index of
/// projects in `<xdg-data>/crush/projects.json`. A root may be that data dir (or its
/// `projects.json`), a project dir, a `.crush` dir, or a `crush.db` file.
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    let mut seen_dbs: HashSet<PathBuf> = HashSet::new();
    for (db_path, project) in db_files(root) {
        let canon = db_path.canonicalize().unwrap_or_else(|_| db_path.clone());
        if !seen_dbs.insert(canon) {
            continue;
        }
        out.extend(discover_db(&db_path, project.as_deref(), opts)?);
    }
    Ok(out)
}

/// `(crush.db path, project dir if known)` pairs reachable from `root`.
fn db_files(root: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    if root.is_file() {
        return match root.file_name().and_then(|s| s.to_str()) {
            Some("projects.json") => read_projects(root),
            _ => vec![(root.to_path_buf(), project_of_db(root))],
        };
    }
    let direct = root.join("crush.db");
    if direct.is_file() {
        return vec![(direct.clone(), project_of_db(&direct))];
    }
    let dot = root.join(".crush").join("crush.db");
    if dot.is_file() {
        return vec![(dot, Some(root.to_path_buf()))];
    }
    let index = root.join("projects.json");
    if index.is_file() {
        return read_projects(&index);
    }
    Vec::new()
}

fn project_of_db(db: &Path) -> Option<PathBuf> {
    // <project>/.crush/crush.db
    let dir = db.parent()?;
    (dir.file_name().and_then(|s| s.to_str()) == Some(".crush"))
        .then(|| dir.parent().map(Path::to_path_buf))
        .flatten()
}

fn read_projects(index: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let Ok(raw) = std::fs::read_to_string(index) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in v
        .get("projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(data_dir) = p.get("data_dir").and_then(Value::as_str) else {
            continue;
        };
        let db = Path::new(data_dir).join("crush.db");
        if db.is_file() {
            let project = p.get("path").and_then(Value::as_str).map(PathBuf::from);
            out.push((db, project));
        }
    }
    out
}

fn discover_db(
    db_path: &Path,
    project: Option<&Path>,
    opts: &ParseOptions,
) -> Result<Vec<SourceCandidate>> {
    let conn = open_live_sqlite(db_path)?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.parent_session_id, COUNT(m.id), COALESCE(MAX(m.updated_at), 0), s.updated_at
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id ORDER BY s.created_at",
        )
        .with_context(|| format!("{} is not a Crush database", db_path.display()))?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let db_hash = short_sha256(&db_path.to_string_lossy());
    let mut out = Vec::new();
    for row in rows {
        let (sid, parent, count, max_updated, s_updated) = row?;
        if count == 0
            || (!opts.include_subagents && parent.as_deref().is_some_and(|p| !p.is_empty()))
        {
            continue;
        }
        // Project directory is recorded in the candidate path suffix `#<sid>` plus the db location.
        let _ = project;
        out.push(SourceCandidate {
            agent: AgentKind::Crush,
            item_key: format!("crush/{db_hash}/{}", safe_component(&sid, 120)),
            version: format!("crdb:{count}:{max_updated}:{s_updated}"),
            source_path: format!("{}#{sid}", db_path.display()),
            fs_path: db_path.to_path_buf(),
            inner_id: Some(sid),
        });
    }
    Ok(out)
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(sid) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    let db_path = &candidate.fs_path;
    let conn = open_live_sqlite(db_path)?;
    let meta: Option<(Option<String>, String, i64, i64)> = conn
        .query_row(
            "SELECT parent_session_id, title, created_at, updated_at FROM sessions WHERE id = ?1",
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    let Some((parent, title, created, updated)) = meta else {
        return Ok(ParseOutcome::Skipped {
            reason: "session_missing",
        });
    };
    let is_subagent = parent.as_deref().is_some_and(|p| !p.is_empty());
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    // The project is the directory that holds `.crush/`.
    let cwd = project_of_db(db_path).map(|p| p.to_string_lossy().into_owned());
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut stmt = conn.prepare(
        "SELECT id, role, parts, created_at FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant') AND is_summary_message = 0
         ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map(params![sid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (id, role, parts, ts) = row?;
        let role = if role == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        push_turn_dedup(
            &mut turns,
            role,
            text_of_parts(&parts),
            ms_to_rfc3339(ts * 1000),
            SourceOffset::Item { item_id: id },
        );
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Crush,
        session_id: sid.to_string(),
        source_path: candidate.source_path.clone(),
        cwd,
        started_at: ms_to_rfc3339(created * 1000),
        ended_at: ms_to_rfc3339(updated * 1000),
        title: Some(title).filter(|t| !t.trim().is_empty() && t != "New Session"),
        is_subagent,
        parent_session_id: parent.filter(|p| !p.is_empty()),
        turns,
    }))
}

/// Visible text of a message: only `{"type":"text"}` parts. `reasoning`, `tool_call`,
/// `tool_result`, `binary` and `finish` parts are not dialogue.
fn text_of_parts(parts_json: &str) -> String {
    let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(parts_json) else {
        return String::new();
    };
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("data")?.get("text")?.as_str())
        .map(strip_injected_system_blocks)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Raw records behind a turn: the message itself plus, for an assistant reply, the
/// tool-use/tool messages since the previous user/assistant text message.
pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    let (db_str, sid) = source_path
        .rsplit_once('#')
        .context("crush source path has no #<session id>")?;
    let conn: Connection = open_live_sqlite(Path::new(db_str))?;
    let (role, rowid): (String, i64) = conn.query_row(
        "SELECT role, rowid FROM messages WHERE session_id = ?1 AND id = ?2",
        params![sid, item_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // `created_at` has one-second resolution, so a prompt and its reply usually tie; the
    // insertion order (`rowid`) is the reliable ordering.
    let lower: i64 = if role == "assistant" {
        conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM messages
             WHERE session_id = ?1 AND role IN ('user', 'assistant') AND rowid < ?2
               AND parts LIKE '%\"type\":\"text\"%'",
            params![sid, rowid],
            |r| r.get(0),
        )?
    } else {
        rowid - 1
    };
    let mut stmt = conn.prepare(
        "SELECT id, role, parts, model, created_at FROM messages
         WHERE session_id = ?1 AND rowid > ?2 AND rowid <= ?3 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![sid, lower, rowid], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?,
            "role": r.get::<_, String>(1)?,
            "parts": serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
            "model": r.get::<_, Option<String>>(3)?,
            "created_at": r.get::<_, i64>(4)?,
        }))
    })?;
    let records: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
    Ok(serde_json::to_string_pretty(&records)?)
}
