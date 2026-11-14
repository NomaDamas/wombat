pub mod aider;
pub mod aside;
pub mod claude_code;
pub mod codebuddy;
pub mod codex;
pub mod continue_cli;
pub mod copilot;
pub mod crush;
pub mod droid;
pub mod gemini;
pub mod goose;
pub mod hermes;
pub mod opencode;
pub mod pi;
pub mod qwen;
pub mod vibe;

use crate::mirror::{safe_component, short_sha256};
use crate::model::{AgentKind, ParsedSession, Role, SourceCandidate, SourceOffset, Turn};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const PARSER_FINGERPRINT: &str = "wombat-parsers-v9";

#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub include_subagents: bool,
    pub include_ephemeral: bool,
}

#[derive(Debug, Clone)]
pub enum ParseOutcome {
    Session(ParsedSession),
    Skipped { reason: &'static str },
}

#[derive(Debug, Clone, Default)]
pub struct SourceRoots {
    pub codex: Vec<PathBuf>,
    pub claude_code: Vec<PathBuf>,
    pub opencode: Vec<PathBuf>,
    pub pi: Vec<PathBuf>,
    pub omp: Vec<PathBuf>,
    pub hermes: Vec<PathBuf>,
    pub aside: Vec<PathBuf>,
    pub droid: Vec<PathBuf>,
    pub goose: Vec<PathBuf>,
    pub vibe: Vec<PathBuf>,
    pub aider: Vec<PathBuf>,
    pub codebuddy: Vec<PathBuf>,
    pub iflow: Vec<PathBuf>,
    pub continue_cli: Vec<PathBuf>,
    pub copilot: Vec<PathBuf>,
    pub crush: Vec<PathBuf>,
    pub qwen: Vec<PathBuf>,
    pub kilo: Vec<PathBuf>,
    pub gemini: Vec<PathBuf>,
}

impl SourceRoots {
    pub fn default_from_env() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let mut roots = Self::default();
        if let Some(h) = &home {
            // Codex
            let codex_home = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".codex"));
            push_if_exists(&mut roots.codex, codex_home.join("sessions"));
            push_if_exists(&mut roots.codex, codex_home.join("archived_sessions"));

            // Claude Code
            let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".claude"));
            push_if_exists(&mut roots.claude_code, claude_home.join("projects"));

            // OpenCode
            let oc_dir = std::env::var_os("XDG_DATA_HOME")
                .map(|x| PathBuf::from(x).join("opencode"))
                .unwrap_or_else(|| h.join(".local").join("share").join("opencode"));
            push_if_exists(&mut roots.opencode, oc_dir);

            // Pi
            push_if_exists(&mut roots.pi, h.join(".pi").join("agent").join("sessions"));

            // Hermes Agent
            let hermes_home = std::env::var_os("HERMES_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".hermes"));
            push_if_exists(&mut roots.hermes, hermes_home);

            // Gemini CLI
            let gemini_home = std::env::var_os("GEMINI_CLI_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.clone());
            push_if_exists(&mut roots.gemini, gemini_home.join(".gemini"));

            // Kilo Code CLI (OpenCode fork)
            let kilo_dir = std::env::var_os("XDG_DATA_HOME")
                .map(|x| PathBuf::from(x).join("kilo"))
                .unwrap_or_else(|| h.join(".local").join("share").join("kilo"));
            push_if_exists(&mut roots.kilo, kilo_dir);

            // Qwen Code
            let qwen_home = std::env::var_os("QWEN_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".qwen"));
            push_if_exists(&mut roots.qwen, qwen_home);

            // Crush (Charm): the shared project index; each project keeps its own `.crush/crush.db`.
            let crush_share = std::env::var_os("XDG_DATA_HOME")
                .map(|x| PathBuf::from(x).join("crush"))
                .unwrap_or_else(|| h.join(".local").join("share").join("crush"));
            push_if_exists(&mut roots.crush, crush_share);

            // Copilot CLI
            let copilot_home = std::env::var_os("COPILOT_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".copilot"));
            push_if_exists(&mut roots.copilot, copilot_home);

            // Continue CLI
            let continue_cli_home = std::env::var_os("CONTINUE_GLOBAL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".continue"));
            push_if_exists(&mut roots.continue_cli, continue_cli_home);

            // iFlow CLI
            push_if_exists(&mut roots.iflow, h.join(".iflow").join("projects"));

            // CodeBuddy
            let codebuddy_home = std::env::var_os("CODEBUDDY_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".codebuddy"));
            push_if_exists(&mut roots.codebuddy, codebuddy_home);

            // Mistral Vibe
            let vibe_home = std::env::var_os("VIBE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".vibe"));
            push_if_exists(&mut roots.vibe, vibe_home);

            // Goose (sessions.db under the Goose data dir)
            let goose_data = std::env::var_os("GOOSE_PATH_ROOT")
                .map(|r| PathBuf::from(r).join("data"))
                .or_else(|| {
                    std::env::var_os("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("goose"))
                })
                .unwrap_or_else(|| h.join(".local").join("share").join("goose"));
            push_if_exists(&mut roots.goose, goose_data);

            // Droid
            let droid_home = std::env::var_os("FACTORY_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".factory"));
            push_if_exists(&mut roots.droid, droid_home);

            // Aside browser agent
            push_if_exists(&mut roots.aside, h.join(".aside"));

            // OMP (Oh-My-Pi)
            push_if_exists(
                &mut roots.omp,
                h.join(".omp").join("agent").join("sessions"),
            );
        }
        roots
    }

    pub fn roots_for(&self, agent: AgentKind) -> &[PathBuf] {
        match agent {
            AgentKind::Codex => &self.codex,
            AgentKind::ClaudeCode => &self.claude_code,
            AgentKind::OpenCode => &self.opencode,
            AgentKind::Pi => &self.pi,
            AgentKind::Omp => &self.omp,
            AgentKind::Hermes => &self.hermes,
            AgentKind::Aside => &self.aside,
            AgentKind::Droid => &self.droid,
            AgentKind::Goose => &self.goose,
            AgentKind::Vibe => &self.vibe,
            AgentKind::Aider => &self.aider,
            AgentKind::Codebuddy => &self.codebuddy,
            AgentKind::Iflow => &self.iflow,
            AgentKind::Continue => &self.continue_cli,
            AgentKind::Copilot => &self.copilot,
            AgentKind::Crush => &self.crush,
            AgentKind::Qwen => &self.qwen,
            AgentKind::Kilo => &self.kilo,
            AgentKind::Gemini => &self.gemini,
        }
    }
}

/// An iFlow root may be `~/.iflow` or its `projects/` dir.
fn iflow_projects(root: &Path) -> PathBuf {
    let p = root.join("projects");
    if p.is_dir() { p } else { root.to_path_buf() }
}

fn push_if_exists(vec: &mut Vec<PathBuf>, p: PathBuf) {
    if p.exists() {
        vec.push(p);
    }
}

pub fn discover_all(
    roots: &SourceRoots,
    agents_filter: Option<&[AgentKind]>,
    opts: &ParseOptions,
) -> Result<Vec<SourceCandidate>> {
    let mut out = Vec::new();
    for agent in AgentKind::ALL {
        if let Some(allowed) = agents_filter
            && !allowed.contains(&agent)
        {
            continue;
        }
        for root in roots.roots_for(agent) {
            let discovered = match agent {
                AgentKind::Codex => codex::discover(root)?,
                AgentKind::ClaudeCode => claude_code::discover(root, opts)?,
                AgentKind::OpenCode => opencode::discover(root, opts)?,
                AgentKind::Pi => pi::discover(root, AgentKind::Pi, opts)?,
                AgentKind::Omp => pi::discover(root, AgentKind::Omp, opts)?,
                AgentKind::Hermes => hermes::discover(root, opts)?,
                AgentKind::Aside => aside::discover(root)?,
                AgentKind::Droid => droid::discover(root)?,
                AgentKind::Goose => goose::discover(root, opts)?,
                AgentKind::Vibe => vibe::discover(root, opts)?,
                AgentKind::Aider => aider::discover(root)?,
                AgentKind::Codebuddy => codebuddy::discover(root)?,
                AgentKind::Iflow => {
                    claude_code::discover_as(AgentKind::Iflow, &iflow_projects(root), opts)?
                }
                AgentKind::Continue => continue_cli::discover(root)?,
                AgentKind::Copilot => copilot::discover(root)?,
                AgentKind::Crush => crush::discover(root, opts)?,
                AgentKind::Qwen => qwen::discover(root)?,
                AgentKind::Kilo => opencode::discover_kilo(root, opts)?,
                AgentKind::Gemini => gemini::discover(root, opts)?,
            };
            out.extend(discovered);
        }
    }
    out.sort_by(|a, b| a.item_key.cmp(&b.item_key));
    out.dedup_by(|a, b| a.item_key == b.item_key);
    Ok(out)
}

pub fn parse_candidate(candidate: &SourceCandidate, opts: &ParseOptions) -> Result<ParseOutcome> {
    match candidate.agent {
        AgentKind::Codex => codex::parse_file(&candidate.fs_path, opts),
        AgentKind::ClaudeCode => claude_code::parse_file(&candidate.fs_path, opts),
        AgentKind::OpenCode | AgentKind::Kilo => opencode::parse_candidate(candidate, opts),
        AgentKind::Pi => pi::parse_file(&candidate.fs_path, AgentKind::Pi, opts),
        AgentKind::Omp => pi::parse_file(&candidate.fs_path, AgentKind::Omp, opts),
        AgentKind::Hermes => hermes::parse_candidate(candidate, opts),
        AgentKind::Aside => aside::parse_file(candidate, opts),
        AgentKind::Droid => droid::parse_file(&candidate.fs_path, opts),
        AgentKind::Goose => goose::parse_candidate(candidate, opts),
        AgentKind::Vibe => vibe::parse_candidate(candidate, opts),
        AgentKind::Aider => aider::parse_candidate(candidate, opts),
        AgentKind::Codebuddy => codebuddy::parse_file(&candidate.fs_path, opts),
        AgentKind::Iflow => claude_code::parse_file_as(AgentKind::Iflow, &candidate.fs_path, opts),
        AgentKind::Continue => continue_cli::parse_file(&candidate.fs_path, opts),
        AgentKind::Copilot => copilot::parse_file(&candidate.fs_path, opts),
        AgentKind::Crush => crush::parse_candidate(candidate, opts),
        AgentKind::Qwen => qwen::parse_file(&candidate.fs_path, opts),
        AgentKind::Gemini => gemini::parse_file(&candidate.fs_path, opts),
    }
}

/// Extract raw source trace records for a given session source_path and `SourceOffset`.
pub fn extract_raw_trace(
    agent: AgentKind,
    source_path: &str,
    offset: &SourceOffset,
    context_lines: u64,
) -> Result<String> {
    match offset {
        SourceOffset::Lines {
            start_line,
            end_line,
        } => {
            let path = Path::new(source_path);
            let reader = open_jsonl_reader(path)?;
            let lo = start_line.saturating_sub(context_lines).max(1);
            let hi = end_line.saturating_add(context_lines);
            let mut out = String::new();
            for (idx, line) in reader.lines().enumerate() {
                let line_no = (idx + 1) as u64;
                if line_no > hi {
                    break;
                }
                let line = line?;
                if line_no >= lo {
                    out.push_str(&format!("{line_no}: {line}\n"));
                }
            }
            Ok(out)
        }
        SourceOffset::Bytes {
            start_byte,
            end_byte,
        } => {
            let bytes = std::fs::read(source_path)
                .with_context(|| format!("failed reading {source_path}"))?;
            let s = (*start_byte as usize).min(bytes.len());
            let e = (*end_byte as usize).min(bytes.len()).max(s);
            Ok(String::from_utf8_lossy(&bytes[s..e]).into_owned())
        }
        SourceOffset::Item { item_id } => match agent {
            AgentKind::OpenCode | AgentKind::Kilo => {
                opencode::extract_raw_item(source_path, item_id)
            }
            AgentKind::Hermes => hermes::extract_raw_item(source_path, item_id),
            AgentKind::Goose => goose::extract_raw_item(source_path, item_id),
            AgentKind::Vibe => vibe::extract_raw_item(source_path, item_id),
            AgentKind::Continue => continue_cli::extract_raw_item(source_path, item_id),
            AgentKind::Crush => crush::extract_raw_item(source_path, item_id),
            _ => Ok(format!("source_path={source_path} item_id={item_id}")),
        },
    }
}

/// Open either a plain `.jsonl` or a `.jsonl.zst` zstd-compressed file as a streaming `BufRead`.
pub fn open_jsonl_reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let is_zst = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zst"));
    if is_zst {
        let decoder = zstd::stream::read::Decoder::new(file)
            .with_context(|| format!("failed to init zstd decoder for {}", path.display()))?;
        Ok(Box::new(BufReader::with_capacity(64 * 1024, decoder)))
    } else {
        Ok(Box::new(BufReader::with_capacity(64 * 1024, file)))
    }
}

pub fn file_version_token(path: &Path) -> Result<String> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("failed stating {}", path.display()))?;
    let size = meta.len();
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(format!("fs:{size}:{mtime_ns}"))
}

pub fn build_file_item_key(agent: AgentKind, path: &Path) -> String {
    let full = path.to_string_lossy();
    let hash = short_sha256(&full);
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    format!("{}/{}/{}", agent.as_str(), hash, safe_component(fname, 120))
}

/// Coalesce consecutive turns only when separated by noise? No: preserve distinct user/assistant
/// messages cleanly, or merge consecutive assistant commentary/final blocks? Wait: keeping each
/// message as its own Turn preserves exact per-message line offsets and keeps prefix-stability
/// trivial when a new message is appended! Dedup exact consecutive duplicate user prompts (e.g.
/// Claude Code queued-prompt echo on identical timestamp).
pub fn push_turn_dedup(
    turns: &mut Vec<Turn>,
    role: Role,
    text: String,
    timestamp: Option<String>,
    source_offset: SourceOffset,
) {
    let cleaned = text.trim().to_string();
    if cleaned.is_empty() {
        return;
    }
    if role == Role::User
        && let Some(prev) = turns.last()
        && prev.role == Role::User
        && prev.text == cleaned
        && prev.timestamp == timestamp
    {
        return;
    }
    turns.push(Turn {
        role,
        text: cleaned,
        timestamp,
        source_offset,
    });
}

/// Strip leading/embedded system-reminder XML blocks injected into user/assistant turns by harnesses.
pub fn strip_injected_system_blocks(raw: &str) -> String {
    const TAGS: &[&str] = &[
        "system-reminder",
        "system_reminder",
        "system-notice",
        "system_notice",
        "attachment",
        "local-command-caveat",
        "local-command-stdout",
        "command-name",
        "command-message",
        "command-args",
        "task-notification",
        "environment_context",
        "app-context",
        "skills_instructions",
        "permissions instructions",
        "multi_agent_role",
        "multi_agent_mode",
        "codex_apps_client_time_context",
        "external_codex_apps_open_page",
        "codex_apps_open_page_instructions",
        "turn_aborted",
    ];
    let mut cur = raw.to_string();
    for tag in TAGS {
        let open_pat = format!("<{tag}");
        let close_pat = format!("</{tag}>");
        while let Some(start) = cur.find(&open_pat) {
            // Confirm tag boundary (`>` or whitespace right after tag name)
            let after_name = start + open_pat.len();
            let valid_open = cur[after_name..]
                .chars()
                .next()
                .is_some_and(|c| c == '>' || c.is_whitespace());
            if !valid_open {
                break;
            }
            if let Some(rel_end) = cur[after_name..].find(&close_pat) {
                let remove_end = after_name + rel_end + close_pat.len();
                cur.replace_range(start..remove_end, "");
            } else {
                break;
            }
        }
    }
    // Strip HTML comment markers like `<!-- OMO_INTERNAL_INITIATOR -->`
    while let Some(s) = cur.find("<!-- OMO_INTERNAL") {
        if let Some(rel_e) = cur[s..].find("-->") {
            cur.replace_range(s..(s + rel_e + 3), "");
        } else {
            break;
        }
    }
    cur.trim().to_string()
}

/// Open a SQLite database another application keeps open in WAL mode, without copying it.
/// A plain read-only connection sees WAL content; only if that fails (e.g. no `-shm` and a
/// read-only directory) fall back to a temp snapshot copy.
pub fn open_live_sqlite(db_path: &Path) -> Result<rusqlite::Connection> {
    if let Ok(conn) = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) && conn.busy_timeout(std::time::Duration::from_secs(5)).is_ok()
        && conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .is_ok()
    {
        return Ok(conn);
    }
    Ok(open_readonly_sqlite(db_path)?.conn)
}

/// Open another application's potentially WAL-mode SQLite database read-only without mutating its directory.
/// If a non-empty `-wal` file sits beside `db_path`, copies `db`, `-wal`, and `-shm` to a temp dir first.
/// Not using `tempfile` in production library deps — build a tiny RAII temp copy dir under `std::env::temp_dir()`.
pub struct TempSnapshotDir(PathBuf);

impl Drop for TempSnapshotDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct ReadonlySqlite {
    pub conn: rusqlite::Connection,
    _snap: Option<TempSnapshotDir>,
}

pub fn open_readonly_sqlite(db_path: &Path) -> Result<ReadonlySqlite> {
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let has_wal = std::fs::metadata(&wal_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);

    if !has_wal {
        let uri = format!("file:{}?immutable=1", db_path.display());
        let conn = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .or_else(|_| {
            rusqlite::Connection::open_with_flags(
                db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
        })
        .with_context(|| format!("failed opening sqlite {}", db_path.display()))?;
        return Ok(ReadonlySqlite { conn, _snap: None });
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let snap_dir =
        std::env::temp_dir().join(format!("wombat-sqlite-{}-{}", std::process::id(), nonce));
    std::fs::create_dir_all(&snap_dir)?;
    let snap_guard = TempSnapshotDir(snap_dir.clone());
    let dst_db = snap_dir.join("db.sqlite");
    std::fs::copy(db_path, &dst_db)?;
    let _ = std::fs::copy(&wal_path, snap_dir.join("db.sqlite-wal"));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    if shm_path.is_file() {
        let _ = std::fs::copy(&shm_path, snap_dir.join("db.sqlite-shm"));
    }
    let conn = rusqlite::Connection::open(&dst_db)
        .with_context(|| format!("failed opening snapshot sqlite {}", dst_db.display()))?;
    Ok(ReadonlySqlite {
        conn,
        _snap: Some(snap_guard),
    })
}
