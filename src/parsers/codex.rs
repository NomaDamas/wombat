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
use std::path::Path;
use walkdir::WalkDir;

pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
            continue;
        }
        let Ok(version) = file_version_token(path) else {
            continue;
        };
        out.push(SourceCandidate {
            agent: AgentKind::Codex,
            item_key: build_file_item_key(AgentKind::Codex, path),
            version,
            source_path: path.to_string_lossy().into_owned(),
            fs_path: path.to_path_buf(),
            inner_id: None,
        });
    }
    Ok(out)
}

pub fn parse_file(path: &Path, opts: &ParseOptions) -> Result<ParseOutcome> {
    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut ended_at: Option<String> = None;
    let mut is_subagent = false;
    let mut parent_session_id: Option<String> = None;
    let mut is_headless = false;

    let mut turns: Vec<Turn> = Vec::new();
    let mut fallback_turns: Vec<Turn> = Vec::new();

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
            .map(ToString::to_string);
        if let Some(t) = &ts {
            if started_at.is_none() {
                started_at = Some(t.clone());
            }
            ended_at = Some(t.clone());
        }

        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        match record_type {
            "session_meta" => {
                if let Some(payload) = v.get("payload") {
                    if let Some(id) = payload
                        .get("session_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                    {
                        session_id = Some(id.to_string());
                    }
                    if let Some(c) = payload.get("cwd").and_then(Value::as_str) {
                        cwd = Some(c.to_string());
                    }
                    if matches!(
                        payload.get("originator").and_then(Value::as_str),
                        Some("codex_exec" | "codex_sdk_ts")
                    ) {
                        is_headless = true;
                    }
                    if let Some(meta_ts) = payload.get("timestamp").and_then(Value::as_str) {
                        started_at = Some(meta_ts.to_string());
                    }
                    if let Some(tsrc) = payload.get("thread_source") {
                        if tsrc.as_str() == Some("subagent") {
                            is_subagent = true;
                        } else if tsrc.is_object() && tsrc.get("subagent").is_some() {
                            is_subagent = true;
                            parent_session_id = tsrc
                                .pointer("/subagent/thread_spawn/parent_thread_id")
                                .and_then(Value::as_str)
                                .map(ToString::to_string);
                        }
                    }
                    if let Some(src) = payload.get("source")
                        && src.is_object()
                        && src.get("subagent").is_some()
                    {
                        is_subagent = true;
                    }
                }
            }
            "response_item" => {
                let Some(payload) = v.get("payload") else {
                    continue;
                };
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    // Explicitly ignore reasoning, custom_tool_call, custom_tool_call_output,
                    // function_call, function_call_output, local_shell_call, web_search_call, etc.
                    continue;
                }
                let role_str = payload.get("role").and_then(Value::as_str).unwrap_or("");
                let role = match role_str {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => continue, // skip developer / system
                };
                let text = extract_codex_message_text(payload, role);
                push_turn_dedup(
                    &mut turns,
                    role,
                    text,
                    ts,
                    SourceOffset::Lines {
                        start_line: line_no,
                        end_line: line_no,
                    },
                );
            }
            "event_msg" => {
                // Fallback only for very old rollouts that lacked response_item messages.
                let Some(payload) = v.get("payload") else {
                    continue;
                };
                let ev_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                match ev_type {
                    "user_message" => {
                        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                            let cleaned = clean_codex_user_part(msg);
                            push_turn_dedup(
                                &mut fallback_turns,
                                Role::User,
                                cleaned,
                                ts,
                                SourceOffset::Lines {
                                    start_line: line_no,
                                    end_line: line_no,
                                },
                            );
                        }
                    }
                    "agent_message" => {
                        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
                            push_turn_dedup(
                                &mut fallback_turns,
                                Role::Assistant,
                                strip_injected_system_blocks(msg),
                                ts,
                                SourceOffset::Lines {
                                    start_line: line_no,
                                    end_line: line_no,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if is_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }
    if is_headless && !opts.include_ephemeral {
        return Ok(ParseOutcome::Skipped {
            reason: "headless_exec",
        });
    }
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    if turns.is_empty() && !fallback_turns.is_empty() {
        turns = fallback_turns;
    }
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let resolved_id = session_id.unwrap_or_else(|| derive_codex_id_from_filename(path));
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Codex,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title: None,
        is_subagent,
        parent_session_id,
        turns,
    }))
}

fn extract_codex_message_text(payload: &Value, role: Role) -> String {
    let Some(content) = payload.get("content") else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(s) = content.as_str() {
        let cleaned = match role {
            Role::User => clean_codex_user_part(s),
            Role::Assistant => strip_injected_system_blocks(s),
        };
        if !cleaned.is_empty() {
            parts.push(cleaned);
        }
    } else if let Some(arr) = content.as_array() {
        for part in arr {
            let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
            if !matches!(ptype, "input_text" | "output_text" | "text") {
                continue;
            }
            let Some(raw_text) = part.get("text").and_then(Value::as_str) else {
                continue;
            };
            let cleaned = match role {
                Role::User => clean_codex_user_part(raw_text),
                Role::Assistant => strip_injected_system_blocks(raw_text),
            };
            if !cleaned.is_empty() {
                parts.push(cleaned);
            }
        }
    }
    parts.join("\n\n")
}

fn clean_codex_user_part(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Codex injects AGENTS.md instructions as a user input_text block.
    if trimmed.starts_with("# AGENTS.md instructions")
        || trimmed.starts_with("<INSTRUCTIONS>")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<app-context>")
        || trimmed.starts_with("<skills_instructions>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<external_codex_apps_")
        || trimmed.starts_with("<codex_apps_")
        || trimmed.starts_with("<user_instructions>")
        || trimmed.starts_with("<subagent_notification>")
    {
        return String::new();
    }
    strip_injected_system_blocks(trimmed)
}

fn derive_codex_id_from_filename(path: &Path) -> String {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("codex");
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))
        .unwrap_or(name);
    // `rollout-2026-10-15T21-58-01-<uuid>` -> extract `<uuid>` if 36 chars at end.
    if stem.len() > 37 {
        let candidate_uuid = &stem[stem.len() - 36..];
        if candidate_uuid.chars().filter(|&c| c == '-').count() == 4 {
            return candidate_uuid.to_string();
        }
    }
    stem.to_string()
}
