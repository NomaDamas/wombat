use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
    ms_to_rfc3339,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, build_file_item_key, file_version_token, open_jsonl_reader,
    open_readonly_sqlite, push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Discover Cursor sessions across all three layouts:
/// 1. Cursor CLI `chats/<workspace-hash>/<chat-id>/store.db`
/// 2. Cursor CLI/Editor transcripts `projects/*/agent-transcripts/**/*.jsonl` (deduped against `store.db` chat IDs)
/// 3. Cursor IDE SQLite `state.vscdb` (`globalStorage/state.vscdb`, `workspaceStorage/*/state.vscdb`, or direct `.vscdb`)
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut seen_chat_ids: HashSet<String> = HashSet::new();

    if root.is_file() {
        let name = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "store.db" {
            if let Some(c) = discover_cli_store_db(root, opts)? {
                if let Some(id) = &c.inner_id {
                    seen_chat_ids.insert(id.clone());
                }
                out.push(c);
            }
        } else if name.ends_with(".vscdb") {
            out.extend(discover_vscdb(root, &seen_chat_ids)?);
        } else if name.ends_with(".jsonl")
            && let Ok(ver) = file_version_token(root)
        {
            out.push(SourceCandidate {
                agent: AgentKind::Cursor,
                item_key: build_file_item_key(AgentKind::Cursor, root),
                version: ver,
                source_path: root.to_string_lossy().into_owned(),
                fs_path: root.to_path_buf(),
                inner_id: None,
            });
        }
        return Ok(out);
    }

    // 1. CLI `chats/<ws>/<chat-id>/store.db`
    let chats_dir = root.join("chats");
    if chats_dir.is_dir() {
        for entry in WalkDir::new(&chats_dir)
            .min_depth(3)
            .max_depth(3)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file()
                && entry.file_name() == "store.db"
                && let Some(c) = discover_cli_store_db(entry.path(), opts)?
            {
                if let Some(id) = &c.inner_id {
                    seen_chat_ids.insert(id.clone());
                }
                out.push(c);
            }
        }
    }

    // 2. Transcripts `projects/*/agent-transcripts/**/*.jsonl`
    let projects_dir = root.join("projects");
    if projects_dir.is_dir() {
        for entry in WalkDir::new(&projects_dir)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if !p.components().any(|c| c.as_os_str() == "agent-transcripts") {
                continue;
            }
            if !opts.include_subagents && p.components().any(|c| c.as_os_str() == "subagents") {
                continue;
            }
            let chat_id = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !chat_id.is_empty() && seen_chat_ids.contains(&chat_id) {
                continue;
            }
            if let Ok(ver) = file_version_token(p) {
                if !chat_id.is_empty() {
                    seen_chat_ids.insert(chat_id);
                }
                out.push(SourceCandidate {
                    agent: AgentKind::Cursor,
                    item_key: build_file_item_key(AgentKind::Cursor, p),
                    version: ver,
                    source_path: p.to_string_lossy().into_owned(),
                    fs_path: p.to_path_buf(),
                    inner_id: None,
                });
            }
        }
    }

    // 3. IDE `state.vscdb` under `globalStorage/state.vscdb`, `workspaceStorage/*/state.vscdb`, or direct children.
    let mut vscdbs: Vec<PathBuf> = Vec::new();
    let global_db = root.join("globalStorage").join("state.vscdb");
    if global_db.is_file() {
        vscdbs.push(global_db);
    }
    let ws_root = root.join("workspaceStorage");
    if ws_root.is_dir()
        && let Ok(entries) = std::fs::read_dir(&ws_root)
    {
        for e in entries.flatten() {
            let candidate = e.path().join("state.vscdb");
            if candidate.is_file() {
                vscdbs.push(candidate);
            }
        }
    }
    let direct_vscdb = root.join("state.vscdb");
    if direct_vscdb.is_file() {
        vscdbs.push(direct_vscdb);
    }
    for db_path in vscdbs {
        out.extend(discover_vscdb(&db_path, &seen_chat_ids)?);
    }

    Ok(out)
}

fn discover_cli_store_db(
    store_path: &Path,
    opts: &ParseOptions,
) -> Result<Option<SourceCandidate>> {
    let Some(chat_dir) = store_path.parent() else {
        return Ok(None);
    };
    let chat_id = chat_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cursor-chat")
        .to_string();

    if let Some(meta_file) = read_cli_meta_json(chat_dir)
        && meta_file.is_subagent
        && !opts.include_subagents
    {
        return Ok(None);
    }

    let mut total_size = 0u64;
    let mut max_mtime = 0u128;
    for p in [
        store_path.to_path_buf(),
        PathBuf::from(format!("{}-wal", store_path.display())),
        chat_dir.join("meta.json"),
    ] {
        if let Ok(m) = std::fs::metadata(&p) {
            total_size += m.len();
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            max_mtime = max_mtime.max(mt);
        }
    }

    let path_hash = short_sha256(&store_path.to_string_lossy());
    Ok(Some(SourceCandidate {
        agent: AgentKind::Cursor,
        item_key: format!("cursor/cli-{path_hash}/{}", safe_component(&chat_id, 120)),
        version: format!("curcli:{total_size}:{max_mtime}"),
        source_path: store_path.to_string_lossy().into_owned(),
        fs_path: store_path.to_path_buf(),
        inner_id: Some(chat_id),
    }))
}

fn discover_vscdb(db_path: &Path, seen_chat_ids: &HashSet<String>) -> Result<Vec<SourceCandidate>> {
    let Ok(db) = open_readonly_sqlite(db_path) else {
        return Ok(Vec::new());
    };
    let conn = &db.conn;
    let has_kv: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cursorDiskKV'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !has_kv {
        return Ok(Vec::new());
    }

    // Range scan uses primary-key index (`';'` immediately follows `':'` in ASCII).
    let mut stmt = conn.prepare(
        "SELECT key, value FROM cursorDiskKV WHERE key >= 'composerData:' AND key < 'composerData;'",
    )?;
    let db_hash = short_sha256(&db_path.to_string_lossy());
    let mut out = Vec::new();
    for r in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (key, val_opt) = r?;
        let Some(raw) = val_opt else {
            continue;
        };
        let Some(cid_from_key) = key.strip_prefix("composerData:") else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let cid = meta
            .get("composerId")
            .and_then(Value::as_str)
            .unwrap_or(cid_from_key)
            .to_string();
        if cid.is_empty() || seen_chat_ids.contains(&cid) {
            continue;
        }
        let header_len = meta
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if header_len == 0 {
            continue;
        }
        let updated = meta
            .get("lastUpdatedAt")
            .or_else(|| meta.get("createdAt"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let rev_hash = short_sha256(&raw);
        out.push(SourceCandidate {
            agent: AgentKind::Cursor,
            item_key: format!("cursor/ide-{db_hash}/{}", safe_component(&cid, 120)),
            version: format!("curide:{updated}:{header_len}:{rev_hash}"),
            source_path: format!("{}#{cid}", db_path.display()),
            fs_path: db_path.to_path_buf(),
            inner_id: Some(cid),
        });
    }
    Ok(out)
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let fname = candidate
        .fs_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if fname == "store.db" {
        parse_cli_store(&candidate.fs_path, opts)
    } else if fname.ends_with(".vscdb") {
        let Some(cid) = candidate.inner_id.as_deref() else {
            return Ok(ParseOutcome::Skipped {
                reason: "missing_composer_id",
            });
        };
        parse_vscdb_composer(&candidate.fs_path, cid, opts)
    } else {
        parse_transcript_jsonl(&candidate.fs_path, opts)
    }
}

#[derive(Debug, Default)]
struct CliMetaJson {
    title: Option<String>,
    cwd: Option<String>,
    created_ms: Option<i64>,
    updated_ms: Option<i64>,
    is_subagent: bool,
}

fn read_cli_meta_json(chat_dir: &Path) -> Option<CliMetaJson> {
    let raw = std::fs::read_to_string(chat_dir.join("meta.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(CliMetaJson {
        title: v
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "New Agent")
            .map(ToString::to_string),
        cwd: v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string),
        created_ms: v.get("createdAtMs").and_then(Value::as_i64),
        updated_ms: v.get("updatedAtMs").and_then(Value::as_i64),
        is_subagent: v
            .get("isSubagent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_cli_store(store_path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let chat_dir = store_path.parent().unwrap_or_else(|| Path::new("."));
    let side_meta = read_cli_meta_json(chat_dir).unwrap_or_default();
    if side_meta.is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if !opts.include_ephemeral
        && let Some(c) = side_meta.cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let db = open_readonly_sqlite(store_path)?;
    let conn = &db.conn;
    let Some(store_meta) = read_store_db_meta(conn) else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_meta",
        });
    };

    let parent_id = store_meta
        .pointer("/subagentInfo/rootParentAgentId")
        .or_else(|| store_meta.pointer("/subagentInfo/parentAgentId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let is_subagent = side_meta.is_subagent || parent_id.is_some();
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }

    let chat_id = chat_dir
        .file_name()
        .and_then(|s| s.to_str())
        .or_else(|| store_meta.get("agentId").and_then(Value::as_str))
        .unwrap_or("cursor-chat")
        .to_string();

    let Some(root_hex) = store_meta.get("latestRootBlobId").and_then(Value::as_str) else {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    };
    let Some(root_blob) = fetch_blob(conn, root_hex)? else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_root_blob",
        });
    };

    let mut message_blob_ids: Vec<String> = Vec::new();
    let (archives, _) = decode_pb_fields(&root_blob, 13, 0);
    for arch_digest in archives {
        let arch_hex = hex::encode(arch_digest);
        if let Some(arch_blob) = fetch_blob(conn, &arch_hex)? {
            let (summed, _) = decode_pb_fields(&arch_blob, 1, 0);
            for d in summed {
                message_blob_ids.push(hex::encode(d));
            }
        }
    }
    let (own, pb_started_ms) = decode_pb_fields(&root_blob, 1, 26);
    for d in own {
        message_blob_ids.push(hex::encode(d));
    }

    let created_ms = side_meta
        .created_ms
        .or_else(|| store_meta.get("createdAt").and_then(Value::as_i64))
        .or_else(|| (pb_started_ms > 0).then_some(pb_started_ms as i64));
    let started_at = created_ms.and_then(ms_to_rfc3339);
    let ended_at = side_meta.updated_ms.or(created_ms).and_then(ms_to_rfc3339);
    let title = side_meta.title.or_else(|| {
        store_meta
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "New Agent")
            .map(ToString::to_string)
    });

    let mut turns = Vec::new();
    for blob_id in message_blob_ids {
        let Some(data) = fetch_blob(conn, &blob_id)? else {
            continue;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&data) else {
            continue;
        };
        let role_str = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let role = match role_str {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let text = extract_cursor_message_content(msg.get("content"), role);
        push_turn_dedup(
            &mut turns,
            role,
            text,
            None,
            SourceOffset::Item { item_id: blob_id },
        );
    }

    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Cursor,
        session_id: chat_id,
        source_path: store_path.to_string_lossy().into_owned(),
        cwd: side_meta.cwd,
        started_at,
        ended_at,
        title,
        is_subagent,
        parent_session_id: parent_id,
        turns,
    }))
}

fn parse_vscdb_composer(
    db_path: &Path,
    composer_id: &str,
    opts: &ParseOptions,
) -> Result<ParseOutcome> {
    let db = open_readonly_sqlite(db_path)?;
    let conn = &db.conn;
    let raw_meta: Option<String> = conn
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            params![format!("composerData:{composer_id}")],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw_meta) = raw_meta else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_composer",
        });
    };
    let meta: Value = serde_json::from_str(&raw_meta)?;
    let headers = meta
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if headers.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let mut cwd = meta
        .pointer("/workspaceIdentifier/uri/fsPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    if cwd.is_none() {
        cwd = read_workspace_json_cwd(db_path);
    }

    let created_ms = meta.get("createdAt").and_then(Value::as_i64);
    let updated_ms = meta.get("lastUpdatedAt").and_then(Value::as_i64);
    let title = meta
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let mut bubble_stmt = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1")?;
    let mut turns = Vec::new();
    for h in headers {
        let Some(bid) = h.get("bubbleId").and_then(Value::as_str) else {
            continue;
        };
        let bkey = format!("bubbleId:{composer_id}:{bid}");
        let Ok(Some(braw)) = bubble_stmt
            .query_row(params![&bkey], |r| r.get::<_, Option<String>>(0))
            .optional()
            .map(|o| o.flatten())
        else {
            continue;
        };
        let Ok(bval) = serde_json::from_str::<Value>(&braw) else {
            continue;
        };
        if cwd.is_none()
            && let Some(wdir) = bval.get("workspaceProjectDir").and_then(Value::as_str)
            && !wdir.is_empty()
        {
            cwd = Some(wdir.to_string());
        }
        let btype = bval
            .get("type")
            .and_then(Value::as_i64)
            .or_else(|| h.get("type").and_then(Value::as_i64))
            .unwrap_or(0);
        // type 1 = user, type 2 = assistant. Ignore pure tool/thinking bubbles with empty text.
        let role = match btype {
            1 => Role::User,
            2 => Role::Assistant,
            _ => continue,
        };
        let raw_text = bval
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| bval.get("rawText").and_then(Value::as_str))
            .unwrap_or("");
        let cleaned = match role {
            Role::User => extract_cursor_user_typed(raw_text),
            Role::Assistant => strip_injected_system_blocks(raw_text),
        };
        let ts = bval
            .get("createdAt")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                bval.get("timestamp")
                    .and_then(Value::as_i64)
                    .and_then(ms_to_rfc3339)
            });
        push_turn_dedup(
            &mut turns,
            role,
            cleaned,
            ts,
            SourceOffset::Item {
                item_id: bid.to_string(),
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

    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Cursor,
        session_id: composer_id.to_string(),
        source_path: format!("{}#{composer_id}", db_path.display()),
        cwd,
        started_at: created_ms.and_then(ms_to_rfc3339),
        ended_at: updated_ms.or(created_ms).and_then(ms_to_rfc3339),
        title,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

fn parse_transcript_jsonl(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let is_subagent = path.components().any(|c| c.as_os_str() == "subagents");
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    let reader = open_jsonl_reader(path)?;
    let mut turns = Vec::new();
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
        let role = match v.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let content = v.pointer("/message/content").or_else(|| v.get("content"));
        let text = extract_cursor_message_content(content, role);
        push_turn_dedup(
            &mut turns,
            role,
            text,
            None,
            SourceOffset::Lines {
                start_line: line_no,
                end_line: line_no,
            },
        );
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }
    let sid = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cursor-transcript")
        .to_string();
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Cursor,
        session_id: sid,
        source_path: path.to_string_lossy().into_owned(),
        cwd: None,
        started_at: None,
        ended_at: None,
        title: None,
        is_subagent,
        parent_session_id: None,
        turns,
    }))
}

fn read_workspace_json_cwd(vscdb_path: &Path) -> Option<String> {
    let ws_json = vscdb_path.parent()?.join("workspace.json");
    let raw = std::fs::read_to_string(ws_json).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let folder = v.get("folder").and_then(Value::as_str)?;
    Some(folder.strip_prefix("file://").unwrap_or(folder).to_string())
}

fn read_store_db_meta(conn: &Connection) -> Option<Value> {
    let raw: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
        .ok()?;
    let trimmed = raw.trim();
    let bytes = if trimmed.starts_with('{') {
        trimmed.as_bytes().to_vec()
    } else {
        hex::decode(trimmed).ok()?
    };
    serde_json::from_slice(&bytes).ok()
}

fn fetch_blob(conn: &Connection, id_hex: &str) -> Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT data FROM blobs WHERE id = ?1",
        params![id_hex],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .optional()
    .map_err(Into::into)
}

fn extract_cursor_message_content(content: Option<&Value>, role: Role) -> String {
    let Some(content) = content else {
        return String::new();
    };
    let joined = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        let mut texts = Vec::new();
        for p in arr {
            // Keep ONLY `text` — skip `reasoning`, `tool-call`, `tool-result`, `tool_use`, `tool_result`.
            if p.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = p.get("text").and_then(Value::as_str)
            {
                texts.push(t);
            }
        }
        texts.join("\n")
    } else {
        return String::new();
    };

    match role {
        Role::User => extract_cursor_user_typed(&joined),
        Role::Assistant => strip_injected_system_blocks(&joined),
    }
}

/// Extract what the user actually typed in a Cursor user message:
/// if `<user_query>...</user_query>` tags are present, extract their inner text;
/// otherwise strip Cursor's injected context tags (`<user_info>`, `<rules>`, `<timestamp>`, etc.).
fn extract_cursor_user_typed(raw: &str) -> String {
    if let Some(extracted) = extract_tag_contents(raw, "user_query") {
        return extracted;
    }
    let mut cleaned = raw.to_string();
    for tag in [
        "timestamp",
        "user_info",
        "project_layout",
        "rules",
        "always_applied_workspace_rules",
        "agent_requestable_workspace_rules",
        "user_rules",
        "agent_skills",
        "available_skills",
        "open_and_recently_viewed_files",
        "mcp_instructions",
        "mcp_file_system",
        "mcp_file_system_servers",
        "git_status",
        "agent_transcripts",
        "cursor_rules_context",
        "attached_files",
        "system_notification",
        "task_notification",
        "agent_notification",
    ] {
        cleaned = remove_xml_tag(&cleaned, tag);
    }
    let stripped = strip_injected_system_blocks(&cleaned);
    if stripped.starts_with('<') && stripped.ends_with('>') && !stripped.contains('\n') {
        return String::new();
    }
    stripped
}

fn extract_tag_contents(raw: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut parts = Vec::new();
    let mut rest = raw;
    while let Some(s) = rest.find(&open) {
        let after = &rest[s + open.len()..];
        let e = after.find(&close)?;
        let inner = after[..e].trim();
        if !inner.is_empty() {
            parts.push(inner.to_string());
        }
        rest = &after[e + close.len()..];
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn remove_xml_tag(raw: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut cur = raw.to_string();
    while let Some(s) = cur.find(&open) {
        let after = s + open.len();
        let valid = cur[after..]
            .chars()
            .next()
            .is_some_and(|c| c == '>' || c.is_whitespace());
        if !valid {
            break;
        }
        if let Some(rel_e) = cur[after..].find(&close) {
            cur.replace_range(s..(after + rel_e + close.len()), "");
        } else {
            break;
        }
    }
    cur
}

/// Minimal schema-free protobuf wire reader matching `agent.v1.ConversationStateStructure`:
/// extracts repeated length-delimited bytes for `bytes_field` (wire type 2) and varint `var_field` (wire type 0).
pub fn decode_pb_fields(mut buf: &[u8], bytes_field: u32, var_field: u32) -> (Vec<Vec<u8>>, u64) {
    let mut vals = Vec::new();
    let mut varint_out = 0u64;
    while !buf.is_empty() {
        let Some((key, n)) = read_uvarint(buf) else {
            break;
        };
        buf = &buf[n..];
        let field_num = (key >> 3) as u32;
        let wire_type = (key & 0x7) as u8;
        match wire_type {
            0 => {
                let Some((v, vn)) = read_uvarint(buf) else {
                    break;
                };
                buf = &buf[vn..];
                if field_num == var_field {
                    varint_out = v;
                }
            }
            1 => {
                if buf.len() < 8 {
                    break;
                }
                buf = &buf[8..];
            }
            2 => {
                let Some((len, ln)) = read_uvarint(buf) else {
                    break;
                };
                buf = &buf[ln..];
                let len = len as usize;
                if buf.len() < len {
                    break;
                }
                if field_num == bytes_field {
                    vals.push(buf[..len].to_vec());
                }
                buf = &buf[len..];
            }
            5 => {
                if buf.len() < 4 {
                    break;
                }
                buf = &buf[4..];
            }
            _ => break,
        }
    }
    (vals, varint_out)
}

fn read_uvarint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut x = 0u64;
    let mut s = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if i >= 10 {
            return None;
        }
        if b < 0x80 {
            if i == 9 && b > 1 {
                return None;
            }
            return Some((x | ((b as u64) << s), i + 1));
        }
        x |= ((b & 0x7f) as u64) << s;
        s += 7;
    }
    None
}

pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    if let Some((db_str, cid)) = source_path.rsplit_once('#')
        && db_str.ends_with(".vscdb")
    {
        let db = open_readonly_sqlite(Path::new(db_str))?;
        let key = format!("bubbleId:{cid}:{item_id}");
        if let Some(val) = db
            .conn
            .query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(val);
        }
    } else if source_path.ends_with("store.db") {
        let db = open_readonly_sqlite(Path::new(source_path))?;
        if let Some(blob) = fetch_blob(&db.conn, item_id)? {
            return Ok(String::from_utf8_lossy(&blob).into_owned());
        }
    }
    Ok(format!("source_path={source_path} item_id={item_id}"))
}
