use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn, is_ephemeral_temp_cwd,
};
use crate::parsers::{
    ParseOptions, ParseOutcome, build_file_item_key, file_version_token, open_jsonl_reader,
    push_turn_dedup, strip_injected_system_blocks,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Discover Kiro CLI sessions under `~/.kiro` (or `$KIRO_HOME`).
///
/// * **V3 (KAS) engine**, the default for interactive chat:
///   `sessions/<workspace-hash>/sess_<id>/{session.json,messages.jsonl}`.
///   Subagent runs are *not* separate sessions: their transcript is
///   `sess_<id>/sub-executions/<sub-session-id>.jsonl`, which is only trace material.
/// * **V2 engine** (`--v2`, and what headless `--no-interactive` used on the machine it was
///   confirmed on): `sessions/cli/<id>.json` (metadata) + `<id>.jsonl` (records).
///
/// A root may be the Kiro home, its `sessions/` dir, one workspace dir, `sessions/cli/`, or a
/// single `sess_<id>` dir.
pub fn discover(root: &Path, _opts: &ParseOptions) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for dir in session_dirs(root) {
        let log = dir.join("messages.jsonl");
        let Some(sid) = dir.file_name().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let Ok(version) = file_version_token(&log) else {
            continue;
        };
        // Include session.json in the version: a rename or a status change must reparse.
        let meta_ver = file_version_token(&dir.join("session.json")).unwrap_or_default();
        out.push(SourceCandidate {
            agent: AgentKind::Kiro,
            item_key: format!(
                "kiro/v3/{}/{}",
                short_sha256(&dir.to_string_lossy()),
                safe_component(&sid, 120)
            ),
            version: format!("{version}:{meta_ver}"),
            source_path: log.to_string_lossy().into_owned(),
            fs_path: log,
            inner_id: Some(sid),
        });
    }
    for jsonl in v2_files(root) {
        let Ok(version) = file_version_token(&jsonl) else {
            continue;
        };
        let meta_ver = file_version_token(&jsonl.with_extension("json")).unwrap_or_default();
        out.push(SourceCandidate {
            agent: AgentKind::Kiro,
            item_key: build_file_item_key(AgentKind::Kiro, &jsonl),
            version: format!("{version}:{meta_ver}"),
            source_path: jsonl.to_string_lossy().into_owned(),
            fs_path: jsonl,
            inner_id: None,
        });
    }
    Ok(out)
}

fn is_session_dir(p: &Path) -> bool {
    p.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with("sess_"))
        && p.join("messages.jsonl").is_file()
}

/// Every `sess_*` dir reachable from `root`.
fn session_dirs(root: &Path) -> Vec<PathBuf> {
    if is_session_dir(root) {
        return vec![root.to_path_buf()];
    }
    let sessions = if root.join("sessions").is_dir() {
        root.join("sessions")
    } else {
        root.to_path_buf()
    };
    let mut out = Vec::new();
    // `sessions/<workspace>/sess_*` or the workspace dir itself.
    let mut candidates = vec![sessions.clone()];
    if let Ok(rd) = std::fs::read_dir(&sessions) {
        let mut ws: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().and_then(|s| s.to_str()) != Some("cli"))
            .collect();
        ws.sort();
        candidates.extend(ws);
    }
    for dir in candidates {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_session_dir(p))
            .collect();
        found.sort();
        out.extend(found);
    }
    out.sort();
    out.dedup();
    out
}

fn v2_files(root: &Path) -> Vec<PathBuf> {
    let cli = if root.file_name().and_then(|s| s.to_str()) == Some("cli") {
        root.to_path_buf()
    } else if root.join("sessions").join("cli").is_dir() {
        root.join("sessions").join("cli")
    } else if root.join("cli").is_dir() {
        root.join("cli")
    } else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&cli) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    if candidate.inner_id.is_some() {
        parse_v3(candidate, opts)
    } else {
        parse_v2(&candidate.fs_path, opts)
    }
}

// ---------------------------------------------------------------------------------------------
// V3
// ---------------------------------------------------------------------------------------------

fn parse_v3(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let path = &candidate.fs_path;
    let dir = path.parent().context("kiro messages.jsonl has no parent")?;
    let meta: Value = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let s = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_string);
    let cwd = meta
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let reader = open_jsonl_reader(path)?;
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
        let Some(payload) = v.get("payload") else {
            continue;
        };
        let role = match payload.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            // `assistant` records are `Say` (visible text) or `Reasoning` (placeholder "..."
            // plus a signature blob); only `Say` is dialogue.
            Some("assistant")
                if payload.get("operationType").and_then(Value::as_str) == Some("Say") =>
            {
                Role::Assistant
            }
            // agent_note (injected <session_context>), tool_call / tool_result, sub_agent_*,
            // session_metadata, usage_summary, session_event, turn_start/end, session_start …
            _ => continue,
        };
        let text = payload
            .get("content")
            .and_then(Value::as_str)
            .map(strip_injected_system_blocks)
            .unwrap_or_default();
        push_turn_dedup(
            &mut turns,
            role,
            text,
            v.get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string),
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
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Kiro,
        session_id: s("id")
            .or_else(|| candidate.inner_id.clone())
            .unwrap_or_else(|| "kiro-session".to_string()),
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at: s("createdAt"),
        ended_at: s("lastModifiedAt"),
        title: s("title")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

// ---------------------------------------------------------------------------------------------
// V2
// ---------------------------------------------------------------------------------------------

fn parse_v2(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let meta: Value = std::fs::read_to_string(path.with_extension("json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let s = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_string);
    let cwd = s("cwd");
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let reader = open_jsonl_reader(path)?;
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
        let role = match v.get("kind").and_then(Value::as_str) {
            Some("Prompt") => Role::User,
            Some("AssistantMessage") => Role::Assistant,
            // ToolResults and anything else is not dialogue
            _ => continue,
        };
        let text = v
            .get("data")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    // `thinking` blocks carry redacted reasoning bytes; `toolUse` has no text
                    .filter(|b| b.get("kind").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("data").and_then(Value::as_str))
                    .map(strip_injected_system_blocks)
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
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
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kiro-session")
        .to_string();
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Kiro,
        session_id: s("session_id").unwrap_or(stem),
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at: s("created_at"),
        ended_at: s("updated_at"),
        title: s("title")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        // `session_created_reason: "subagent"` is what headless `--no-interactive` runs record,
        // so it does not identify a subagent; it is deliberately not used.
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}
