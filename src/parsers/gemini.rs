use crate::mirror::{safe_component, short_sha256};
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

/// Discover Gemini CLI chat logs: `<gemini_home>/tmp/<project>/chats/session-*.jsonl`.
///
/// A root may be `~/.gemini`, its `tmp/` dir, one `tmp/<project>` dir, or a `chats/` dir.
/// The pre-JSONL `session-*.json` snapshots are not read.
pub fn discover(root: &Path, _opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for chats in chats_dirs(root) {
        let Ok(entries) = std::fs::read_dir(&chats) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let is_session = path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl"));
            if !is_session {
                continue;
            }
            let Ok(version) = file_version_token(&path) else {
                continue;
            };
            out.push(SourceCandidate {
                agent: AgentKind::Gemini,
                item_key: build_file_item_key(AgentKind::Gemini, &path),
                version,
                source_path: path.to_string_lossy().into_owned(),
                fs_path: path,
                inner_id: None,
            });
        }
    }
    Ok(out)
}

fn chats_dirs(root: &Path) -> Vec<PathBuf> {
    let name = root.file_name().and_then(|s| s.to_str());
    if name == Some("chats") {
        return vec![root.to_path_buf()];
    }
    let mut dirs = Vec::new();
    let push_project = |dirs: &mut Vec<PathBuf>, project: &Path| {
        let chats = project.join("chats");
        if chats.is_dir() {
            dirs.push(chats);
        }
    };
    let scan_tmp = |tmp: &Path, dirs: &mut Vec<PathBuf>| {
        if let Ok(entries) = std::fs::read_dir(tmp) {
            let mut projects: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            projects.sort();
            for p in projects {
                push_project(dirs, &p);
            }
        }
    };
    if root.join("chats").is_dir() {
        push_project(&mut dirs, root);
    } else if root.join("tmp").is_dir() {
        scan_tmp(&root.join("tmp"), &mut dirs);
    } else {
        scan_tmp(root, &mut dirs);
    }
    dirs
}

/// One logical message after replaying the log, with the source line of its latest record.
struct Msg {
    id: String,
    record: Value,
    line: u64,
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut ended_at: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut directories: Vec<String> = Vec::new();
    let mut msgs: Vec<Msg> = Vec::new();

    // Replays the chat log the way Gemini CLI itself does (chatRecordingService): message
    // records append/replace by id, `$patch` edits or removes messages, `$rewindTo` drops a
    // message and everything after it, and `$set.messages` replaces the whole list.
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
        if let Some(target) = v.get("$rewindTo").and_then(Value::as_str) {
            match msgs.iter().position(|m| m.id == target) {
                Some(pos) => msgs.truncate(pos),
                None => msgs.clear(),
            }
        } else if let Some(patch) = v.get("$patch").filter(|p| p.is_object()) {
            apply_patch(&mut msgs, patch, line_no);
        } else if let Some(set) = v.get("$set").filter(|p| p.is_object()) {
            if let Some(list) = set.get("messages").and_then(Value::as_array) {
                msgs.clear();
                for m in list {
                    upsert(&mut msgs, m, line_no);
                }
            }
            absorb_metadata(
                set,
                &mut session_id,
                &mut started_at,
                &mut ended_at,
                &mut kind,
                &mut directories,
            );
        } else if v.get("id").and_then(Value::as_str).is_some() {
            upsert(&mut msgs, &v, line_no);
        } else if v.get("sessionId").is_some() && v.get("projectHash").is_some() {
            absorb_metadata(
                &v,
                &mut session_id,
                &mut started_at,
                &mut ended_at,
                &mut kind,
                &mut directories,
            );
            if let Some(list) = v.get("messages").and_then(Value::as_array) {
                for m in list {
                    upsert(&mut msgs, m, line_no);
                }
            }
        }
    }

    let is_subagent = kind.as_deref() == Some("subagent");
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    let cwd = directories.first().cloned();
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let mut turns: Vec<Turn> = Vec::new();
    for m in &msgs {
        let role = match m.record.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("gemini") => Role::Assistant,
            // info / error / warning are CLI notices, not dialogue.
            _ => continue,
        };
        let mut text = content_text(m.record.get("content"));
        if role == Role::User && is_ignored_user_text(&text) {
            continue;
        }
        text = strip_injected_system_blocks(&text);
        push_turn_dedup(
            &mut turns,
            role,
            text,
            m.record
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string),
            SourceOffset::Lines {
                start_line: m.line,
                end_line: m.line,
            },
        );
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let resolved_id = session_id.unwrap_or_else(|| {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        format!(
            "{}-{}",
            safe_component(name, 80),
            short_sha256(&path.to_string_lossy())
        )
    });
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Gemini,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title: None,
        is_subagent,
        parent_session_id: None,
        turns,
    }))
}

fn absorb_metadata(
    v: &Value,
    session_id: &mut Option<String>,
    started_at: &mut Option<String>,
    ended_at: &mut Option<String>,
    kind: &mut Option<String>,
    directories: &mut Vec<String>,
) {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    if let Some(id) = s("sessionId") {
        *session_id = Some(id);
    }
    if started_at.is_none() {
        *started_at = s("startTime");
    }
    if let Some(t) = s("lastUpdated") {
        *ended_at = Some(t);
    }
    if let Some(k) = s("kind") {
        *kind = Some(k);
    }
    if let Some(dirs) = v.get("directories").and_then(Value::as_array) {
        *directories = dirs
            .iter()
            .filter_map(|d| d.as_str().map(str::to_string))
            .collect();
    }
}

fn upsert(msgs: &mut Vec<Msg>, record: &Value, line: u64) {
    let Some(id) = record.get("id").and_then(Value::as_str) else {
        return;
    };
    let entry = Msg {
        id: id.to_string(),
        record: record.clone(),
        line,
    };
    match msgs.iter_mut().find(|m| m.id == id) {
        Some(existing) => *existing = entry,
        None => msgs.push(entry),
    }
}

fn apply_patch(msgs: &mut Vec<Msg>, patch: &Value, line: u64) {
    let apply_one = |msgs: &mut Vec<Msg>, p: &Value| {
        let Some(id) = p.get("id").and_then(Value::as_str) else {
            return;
        };
        if let Some(m) = msgs.iter_mut().find(|m| m.id == id)
            && let Some(content) = p.get("content")
        {
            m.record["content"] = content.clone();
            m.line = line;
        }
    };
    apply_one(msgs, patch);
    if let Some(updates) = patch.get("updates").and_then(Value::as_array) {
        for u in updates {
            apply_one(msgs, u);
        }
    }
    if let Some(remove) = patch.get("removeIds").and_then(Value::as_array) {
        let ids: Vec<&str> = remove.iter().filter_map(Value::as_str).collect();
        msgs.retain(|m| !ids.contains(&m.id.as_str()));
    }
    if let Some(order) = patch.get("orderIds").and_then(Value::as_array) {
        let ids: Vec<&str> = order.iter().filter_map(Value::as_str).collect();
        let mut ordered: Vec<Msg> = Vec::new();
        for id in &ids {
            if let Some(pos) = msgs.iter().position(|m| m.id == *id) {
                ordered.push(msgs.remove(pos));
            }
        }
        msgs.extend(ordered);
    }
}

/// User-visible text of a message: a plain string, or the `text` parts of a part list.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Mirrors Gemini CLI's own `isIgnoredUserContent`: empty input, slash/`?` commands, and the
/// injected session/hook context are not user dialogue.
fn is_ignored_user_text(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || t.starts_with('/')
        || t.starts_with('?')
        || t.starts_with("<session_context>")
        || t.starts_with("<hook_context>")
}
