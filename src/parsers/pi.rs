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

/// Discover Pi (`~/.pi/agent/sessions`) or OMP (`~/.omp/agent/sessions`) session files.
///
/// Layout:
/// - `<sessions_root>/<encoded_cwd>/<timestamp>_<uuid>.jsonl` (top-level user session)
/// - `<sessions_root>/<encoded_cwd>/<timestamp>_<uuid>/<SubAgentName>.jsonl` (subagent session, depth 3)
pub fn discover(
    root: &Path,
    agent: AgentKind,
    opts: &ParseOptions,
) -> Result<Vec<SourceCandidate>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for proj_entry in project_dirs.flatten() {
        let Ok(ft) = proj_entry.file_type() else {
            continue;
        };
        let proj_path = proj_entry.path();
        if ft.is_file() {
            // Flat `.jsonl` directly under root (useful in fixtures).
            if proj_path.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && let Ok(version) = file_version_token(&proj_path)
            {
                out.push(SourceCandidate {
                    agent,
                    item_key: build_file_item_key(agent, &proj_path),
                    version,
                    source_path: proj_path.to_string_lossy().into_owned(),
                    fs_path: proj_path,
                    inner_id: None,
                });
            }
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        let dir_name = proj_entry.file_name();
        let dir_str = dir_name.to_string_lossy();
        if !opts.include_ephemeral && is_encoded_ephemeral_dir(&dir_str) {
            continue;
        }

        let Ok(children) = std::fs::read_dir(&proj_path) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(cft) = child.file_type() else {
                continue;
            };
            let cpath = child.path();
            if cft.is_file() {
                if cpath.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(version) = file_version_token(&cpath) else {
                    continue;
                };
                out.push(SourceCandidate {
                    agent,
                    item_key: build_file_item_key(agent, &cpath),
                    version,
                    source_path: cpath.to_string_lossy().into_owned(),
                    fs_path: cpath,
                    inner_id: None,
                });
            } else if cft.is_dir() && opts.include_subagents {
                let Ok(sub_entries) = std::fs::read_dir(&cpath) else {
                    continue;
                };
                for sub in sub_entries.flatten() {
                    let spath = sub.path();
                    if spath.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        continue;
                    }
                    // Skip `<N>.<tool>.log` files; subagents are `.jsonl`.
                    let Ok(version) = file_version_token(&spath) else {
                        continue;
                    };
                    out.push(SourceCandidate {
                        agent,
                        item_key: build_file_item_key(agent, &spath),
                        version,
                        source_path: spath.to_string_lossy().into_owned(),
                        fs_path: spath,
                        inner_id: None,
                    });
                }
            }
        }
    }
    Ok(out)
}

pub fn parse_file(path: &Path, agent: AgentKind, opts: &ParseOptions) -> Result<ParseOutcome> {
    let is_nested_subagent = is_subagent_path(path);
    if is_nested_subagent && !opts.include_subagents {
        return Ok(ParseOutcome::Skipped { reason: "subagent" });
    }

    let reader = open_jsonl_reader(path)?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut ended_at: Option<String> = None;
    let mut title: Option<String> = None;
    let mut is_faux_provider = false;
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
            "session" => {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    session_id = Some(id.to_string());
                }
                if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                    cwd = Some(c.to_string());
                }
                if let Some(t) = v.get("timestamp").and_then(Value::as_str) {
                    started_at = Some(t.to_string());
                }
                if title.is_none()
                    && let Some(t) = v.get("title").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    title = Some(t.trim().to_string());
                }
            }
            "title" | "title_change" => {
                if let Some(t) = v.get("title").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    title = Some(t.trim().to_string());
                }
            }
            "model_change" => {
                if v.get("provider").and_then(Value::as_str) == Some("faux") {
                    is_faux_provider = true;
                }
            }
            "message" => {
                let Some(msg) = v.get("message") else {
                    continue;
                };
                let role_str = msg.get("role").and_then(Value::as_str).unwrap_or("");
                let role = match role_str {
                    "user" => {
                        // OMP marks harness-injected user-like messages with attribution="agent".
                        if msg.get("attribution").and_then(Value::as_str) == Some("agent") {
                            continue;
                        }
                        Role::User
                    }
                    "assistant" => Role::Assistant,
                    // Skip system, developer, toolResult, custom, bashExecution, etc.
                    _ => continue,
                };
                let text = extract_pi_content(msg.get("content"));
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
            _ => {}
        }
    }

    if !opts.include_ephemeral {
        if is_faux_provider {
            return Ok(ParseOutcome::Skipped {
                reason: "faux_automation",
            });
        }
        if let Some(c) = cwd.as_deref()
            && is_ephemeral_temp_cwd(c)
        {
            return Ok(ParseOutcome::Skipped {
                reason: "ephemeral_cwd",
            });
        }
        if turns
            .iter()
            .find(|t| t.role == Role::User)
            .is_some_and(|t| is_harness_prompt(&t.text))
        {
            return Ok(ParseOutcome::Skipped {
                reason: "harness_prompt",
            });
        }
    }

    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }

    let resolved_id = session_id.unwrap_or_else(|| derive_pi_id(path));
    Ok(ParseOutcome::Session(ParsedSession {
        agent,
        session_id: resolved_id,
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at,
        ended_at,
        title,
        is_subagent: is_nested_subagent,
        parent_session_id: None,
        turns,
    }))
}

fn extract_pi_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return strip_injected_system_blocks(s);
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in arr {
        // Keep ONLY `text`; skip `thinking`, `toolCall`, `image`, etc.
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(raw) = block.get("text").and_then(Value::as_str) {
            let cleaned = strip_injected_system_blocks(raw);
            if !cleaned.is_empty() {
                parts.push(cleaned);
            }
        }
    }
    parts.join("\n\n")
}

fn is_encoded_ephemeral_dir(dir_name: &str) -> bool {
    dir_name.starts_with("--tmp-")
        || dir_name == "--tmp--"
        || dir_name.starts_with("--private-tmp-")
        || dir_name.starts_with("--var-folders-")
        || dir_name.starts_with("--private-var-folders-")
}

fn is_subagent_path(path: &Path) -> bool {
    // Subagent files live inside `<timestamp>_<uuid>/<AgentName>.jsonl`, where parent dir is NOT `--...--`.
    let Some(parent_name) = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    let Some(grand_name) = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    grand_name.starts_with('-') && !parent_name.starts_with('-') && parent_name.contains('_')
}

fn derive_pi_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pi-session");
    if let Some((_, uuid)) = stem.rsplit_once('_')
        && uuid.len() >= 8
    {
        return uuid.to_string();
    }
    stem.to_string()
}

/// Prompts AutoRAG's embedded pi librarian is driven with by its own code, not by the user.
fn is_harness_prompt(first_user_text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Answer this original query immediately:",
        "Answer this query directly from your own",
        "Handle this request now, in this one turn",
    ];
    PREFIXES.iter().any(|p| first_user_text.starts_with(p))
}
