use crate::mirror::{safe_component, short_sha256};
use crate::model::{
    AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn, is_ephemeral_temp_cwd,
};
use crate::parsers::{ParseOptions, ParseOutcome, file_version_token, push_turn_dedup};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const HISTORY_FILE: &str = ".aider.chat.history.md";
const SESSION_HEADER: &str = "# aider chat started at ";
/// Aider has no global registry; projects are found by walking down from the root.
const MAX_DEPTH: usize = 4;

/// Discover Aider chat logs: `<project>/.aider.chat.history.md`.
///
/// A root may be a project dir, the history file itself, or a parent directory that contains
/// projects (searched up to a few levels, skipping `.git`, `node_modules`, and similar).
/// One history file holds many sessions (`# aider chat started at <time>` headers), so each
/// header becomes its own candidate (`<file>#<n>`).
pub fn discover(root: &Path) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for file in history_files(root) {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(base_version) = file_version_token(&file) else {
            continue;
        };
        let file_hash = short_sha256(&file.to_string_lossy());
        for (n, _) in split_sessions(&raw).iter().enumerate() {
            let id = n.to_string();
            out.push(SourceCandidate {
                agent: AgentKind::Aider,
                item_key: format!("aider/{file_hash}/{}", safe_component(&id, 40)),
                version: format!("{base_version}:{n}"),
                source_path: file.to_string_lossy().into_owned(),
                fs_path: file.clone(),
                inner_id: Some(id),
            });
        }
    }
    Ok(out)
}

fn history_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_dir()
                && matches!(
                    name.as_ref(),
                    ".git" | "node_modules" | "target" | ".venv" | "venv" | "__pycache__"
                ))
        })
        .flatten()
        .filter(|e| e.file_type().is_file() && e.file_name() == HISTORY_FILE)
        .map(|e| e.into_path())
        .collect();
    files.sort();
    files
}

/// One `# aider chat started at ...` block: its start time, 1-based first line, and body lines.
struct Block {
    started: String,
    first_line: u64,
    lines: Vec<String>,
}

fn split_sessions(raw: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        if let Some(rest) = line.strip_prefix(SESSION_HEADER) {
            blocks.push(Block {
                started: rest.trim().to_string(),
                first_line: (idx + 1) as u64,
                lines: Vec::new(),
            });
        } else if let Some(b) = blocks.last_mut() {
            b.lines.push(line.to_string());
        }
    }
    blocks
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    let Some(id) = candidate.inner_id.as_deref() else {
        return Ok(ParseOutcome::Skipped {
            reason: "missing_session_id",
        });
    };
    let n: usize = id.parse().context("aider session index is not a number")?;
    let path = &candidate.fs_path;
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let blocks = split_sessions(&raw);
    let Some(block) = blocks.get(n) else {
        return Ok(ParseOutcome::Skipped {
            reason: "session_missing",
        });
    };
    let project = path.parent().map(Path::to_path_buf);
    let cwd = project.as_ref().map(|p| p.to_string_lossy().into_owned());
    if !opts.include_ephemeral
        && let Some(c) = cwd.as_deref()
        && is_ephemeral_temp_cwd(c)
    {
        return Ok(ParseOutcome::Skipped {
            reason: "ephemeral_cwd",
        });
    }

    let turns = parse_block(block);
    if turns.is_empty() {
        return Ok(ParseOutcome::Skipped {
            reason: "empty_dialogue",
        });
    }
    // Stable per (project, session start). The file hash keeps two projects' sessions apart.
    let session_id = format!(
        "{}-{}",
        short_sha256(&path.to_string_lossy()),
        safe_component(&block.started, 40)
    );
    Ok(ParseOutcome::Session(ParsedSession {
        agent: AgentKind::Aider,
        session_id,
        // Plain file path: `trace` reads the line ranges recorded in each turn from it.
        source_path: path.to_string_lossy().into_owned(),
        cwd,
        started_at: Some(block.started.replace(' ', "T")),
        ended_at: None,
        title: None,
        is_subagent: false,
        parent_session_id: None,
        turns,
    }))
}

/// Walks one session block. `#### ` lines are what the user typed (consecutive ones are a
/// multi-line prompt); `> ` lines are aider notices / command output; everything else between
/// a prompt and the next one is the assistant's reply.
fn parse_block(block: &Block) -> Vec<Turn> {
    #[derive(PartialEq)]
    enum State {
        Idle,
        User,
        Assistant,
    }
    let mut turns: Vec<Turn> = Vec::new();
    let mut state = State::Idle;
    let mut buf: Vec<String> = Vec::new();
    let (mut start, mut end) = (0u64, 0u64);

    let flush = |turns: &mut Vec<Turn>, role: Role, buf: &mut Vec<String>, start: u64, end: u64| {
        let text = buf.join("\n");
        buf.clear();
        let is_command = role == Role::User && text.trim_start().starts_with('/');
        if is_command {
            return;
        }
        push_turn_dedup(
            turns,
            role,
            text,
            None,
            SourceOffset::Lines {
                start_line: start,
                end_line: end,
            },
        );
    };

    for (i, line) in block.lines.iter().enumerate() {
        let line_no = block.first_line + 1 + i as u64;
        let trimmed_end = line.trim_end();
        if let Some(rest) = trimmed_end.strip_prefix("#### ") {
            if state == State::Assistant {
                flush(&mut turns, Role::Assistant, &mut buf, start, end);
                state = State::Idle;
            }
            if state != State::User {
                start = line_no;
            }
            state = State::User;
            buf.push(rest.to_string());
            end = line_no;
        } else if trimmed_end == "####" {
            if state == State::User {
                buf.push(String::new());
                end = line_no;
            }
        } else if trimmed_end.starts_with("> ") || trimmed_end == ">" {
            // Notices, token accounting and command output end whatever came before them.
            match state {
                State::User => flush(&mut turns, Role::User, &mut buf, start, end),
                State::Assistant => flush(&mut turns, Role::Assistant, &mut buf, start, end),
                State::Idle => {}
            }
            state = State::Idle;
        } else if trimmed_end.is_empty() {
            match state {
                State::User => {
                    flush(&mut turns, Role::User, &mut buf, start, end);
                    state = State::Assistant;
                    buf.clear();
                    start = 0;
                }
                State::Assistant if !buf.is_empty() => {
                    buf.push(String::new());
                }
                _ => {}
            }
        } else {
            match state {
                State::User => {
                    flush(&mut turns, Role::User, &mut buf, start, end);
                    state = State::Assistant;
                    start = line_no;
                    buf.push(line.to_string());
                    end = line_no;
                }
                State::Assistant => {
                    if buf.is_empty() {
                        start = line_no;
                    }
                    buf.push(line.to_string());
                    end = line_no;
                }
                State::Idle => {
                    state = State::Assistant;
                    start = line_no;
                    buf.push(line.to_string());
                    end = line_no;
                }
            }
        }
    }
    match state {
        State::User => flush(&mut turns, Role::User, &mut buf, start, end),
        State::Assistant => flush(&mut turns, Role::Assistant, &mut buf, start, end),
        State::Idle => {}
    }
    turns
}
