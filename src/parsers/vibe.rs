use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, is_ephemeral_temp_cwd,
};
use crate::parsers::{ParseOptions, ParseOutcome, push_turn_dedup, strip_injected_system_blocks};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Discover Mistral Vibe sessions in its unified session store:
/// `<vibe_home>/logs/session/unified/<session-id>/`.
///
/// A store is `CURRENT` (-> generation) + `generations/<g>/manifest.json` + content-addressed
/// `chunks/<sha256>.json`. A root may be `~/.vibe` (or `$VIBE_HOME`), its `logs/`,
/// `logs/session/`, the `unified/` dir, or a single session dir.
pub fn discover(root: &Path, opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for store in store_dirs(root) {
        let Some(cur) = read_current(&store) else {
            continue;
        };
        let sid = store
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        if !opts.include_subagents
            && read_meta(&store)
                .get("parent_session_id")
                .is_some_and(|p| p.as_str().is_some_and(|s| !s.is_empty()))
        {
            continue;
        }
        out.push(SourceCandidate {
            agent: AgentKind::Vibe,
            item_key: format!(
                "vibe/{}/{}",
                short_sha256(&store.to_string_lossy()),
                safe_component(&sid, 120)
            ),
            // The generation id (and its snapshot sequence) changes on every committed write.
            version: format!("vibe:{}:{}", cur.generation, cur.snapshot_sequence),
            source_path: store.to_string_lossy().into_owned(),
            fs_path: store,
            inner_id: Some(sid),
        });
    }
    Ok(out)
}

struct Current {
    generation: String,
    snapshot_sequence: u64,
}

fn read_current(store: &Path) -> Option<Current> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(store.join("CURRENT")).ok()?).ok()?;
    Some(Current {
        generation: v.get("generation")?.as_str()?.to_string(),
        snapshot_sequence: v
            .get("snapshot_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn read_meta(store: &Path) -> Value {
    std::fs::read_to_string(store.join("meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn store_dirs(root: &Path) -> Vec<PathBuf> {
    if root.join("CURRENT").is_file() {
        return vec![root.to_path_buf()];
    }
    let unified = [
        root.join("unified"),
        root.join("session").join("unified"),
        root.join("logs").join("session").join("unified"),
    ]
    .into_iter()
    .find(|p| p.is_dir());
    let base = unified.unwrap_or_else(|| root.to_path_buf());
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("CURRENT").is_file())
        .collect();
    dirs.sort();
    dirs
}

/// The checkpoint's conversation: `history` entries `{message: {role, content}, source, turn_index}`
/// concatenated across the checkpoint's chunks, in order.
fn load_history(store: &Path) -> Result<Vec<Value>> {
    let cur = read_current(store).context("vibe store has no readable CURRENT")?;
    let gen_dir = store.join("generations").join(&cur.generation);
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(gen_dir.join("manifest.json"))
            .with_context(|| format!("no manifest for generation {}", cur.generation))?,
    )?;
    let chunks = manifest
        .get("checkpoint")
        .and_then(|c| c.get("chunks"))
        .and_then(Value::as_array)
        .context("manifest has no checkpoint chunks")?;
    let mut out = Vec::new();
    for digest in chunks.iter().filter_map(Value::as_str) {
        // Chunk ids are lowercase hex digests; refuse anything that could leave the chunk dir.
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let raw = std::fs::read_to_string(store.join("chunks").join(format!("{digest}.json")))
            .with_context(|| format!("missing chunk {digest}"))?;
        if let Value::Array(items) = serde_json::from_str::<Value>(&raw)? {
            out.extend(items);
        }
    }
    Ok(out)
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let store = &candidate.fs_path;
    let meta = read_meta(store);
    let is_subagent = meta
        .get("parent_session_id")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty());
    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    let cwd = meta
        .get("environment")
        .and_then(|e| e.get("working_directory"))
        .and_then(Value::as_str)
        .or_else(|| meta.get("origin_directory").and_then(Value::as_str))
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
    for (idx, entry) in load_history(store)?.iter().enumerate() {
        let Some(msg) = entry.get("message") else {
            continue;
        };
        let role = match msg.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            // system prompt and tool results are not dialogue
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
    let session_id = meta
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| candidate.inner_id.clone())
        .unwrap_or_else(|| "vibe-session".to_string());
    let ts = |k: &str| meta.get(k).and_then(Value::as_str).map(normalize_rfc3339);
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Vibe,
        session_id,
        source_path: store.to_string_lossy().into_owned(),
        cwd,
        started_at: ts("start_time"),
        ended_at: ts("end_time"),
        title: meta
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        is_subagent,
        parent_session_id: meta
            .get("parent_session_id")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string),
        turns,
    }))
}

/// `2026-11-13T14:15:51.772000+00:00` -> `2026-11-13T14:15:51.772Z`.
fn normalize_rfc3339(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| raw.to_string())
}

/// Visible text: only `text` parts (`reasoning`, `tool_call`, images are dropped).
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

/// Raw checkpoint entries behind a turn: its own entry plus, for an assistant reply, the
/// tool-call / tool entries since the previous dialogue message.
pub fn extract_raw_item(source_path: &str, item_id: &str) -> Result<String> {
    let history = load_history(Path::new(source_path))?;
    let idx: usize = item_id.parse().context("vibe item id is not a number")?;
    let role_of = |e: &Value| {
        e.get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let is_dialogue = |e: &Value| {
        matches!(role_of(e).as_deref(), Some("user" | "assistant"))
            && !content_text(e.get("message").and_then(|m| m.get("content")))
                .trim()
                .is_empty()
    };
    let lower = if role_of(history.get(idx).unwrap_or(&Value::Null)).as_deref() == Some("assistant")
    {
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
