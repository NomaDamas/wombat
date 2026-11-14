use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Codex,
    ClaudeCode,
    #[serde(rename = "opencode")]
    OpenCode,
    Pi,
    Omp,
    Hermes,
    Aside,
    Kilo,
    Gemini,
}

impl AgentKind {
    pub const ALL: [AgentKind; 9] = [
        AgentKind::Codex,
        AgentKind::ClaudeCode,
        AgentKind::OpenCode,
        AgentKind::Pi,
        AgentKind::Omp,
        AgentKind::Hermes,
        AgentKind::Aside,
        AgentKind::Kilo,
        AgentKind::Gemini,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::OpenCode => "opencode",
            AgentKind::Pi => "pi",
            AgentKind::Omp => "omp",
            AgentKind::Hermes => "hermes",
            AgentKind::Aside => "aside",
            AgentKind::Kilo => "kilo",
            AgentKind::Gemini => "gemini",
        }
    }

    pub fn parse_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(AgentKind::Codex),
            "claude" | "claude-code" | "claudecode" | "claude_code" => Some(AgentKind::ClaudeCode),
            "opencode" | "open-code" | "open_code" => Some(AgentKind::OpenCode),
            "pi" => Some(AgentKind::Pi),
            "omp" | "oh-my-pi" => Some(AgentKind::Omp),
            "hermes" | "hermes-agent" | "hermes_agent" => Some(AgentKind::Hermes),
            "aside" | "aside-browser" | "aside_browser" => Some(AgentKind::Aside),
            "kilo" | "kilocode" => Some(AgentKind::Kilo),
            "gemini" | "gemini-cli" | "gemini_cli" => Some(AgentKind::Gemini),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn heading_label(self) -> &'static str {
        match self {
            Role::User => "User",
            Role::Assistant => "Assistant",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Stable pointer into the original session trace for one dialogue turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceOffset {
    /// 1-based inclusive line range in a plain or zstd-compressed JSONL file.
    Lines { start_line: u64, end_line: u64 },
    /// Byte span in a plain file.
    Bytes { start_byte: u64, end_byte: u64 },
    /// Stable record / message / bubble identifier in a SQLite or document store.
    Item { item_id: String },
}

impl SourceOffset {
    pub fn compact_label(&self) -> String {
        match self {
            SourceOffset::Lines {
                start_line,
                end_line,
            } => {
                if start_line == end_line {
                    format!("L{start_line}")
                } else {
                    format!("L{start_line}-L{end_line}")
                }
            }
            SourceOffset::Bytes {
                start_byte,
                end_byte,
            } => format!("B{start_byte}-B{end_byte}"),
            SourceOffset::Item { item_id } => format!("id:{}", sanitize_inline(item_id)),
        }
    }

    pub fn parse_compact_label(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Some(rest) = raw.strip_prefix("id:") {
            return (!rest.is_empty()).then(|| SourceOffset::Item {
                item_id: rest.to_string(),
            });
        }
        if let Some(rest) = raw.strip_prefix('L') {
            if let Some((a, b)) = rest.split_once("-L") {
                return Some(SourceOffset::Lines {
                    start_line: a.parse().ok()?,
                    end_line: b.parse().ok()?,
                });
            }
            let line: u64 = rest.parse().ok()?;
            return Some(SourceOffset::Lines {
                start_line: line,
                end_line: line,
            });
        }
        if let Some(rest) = raw.strip_prefix('B') {
            let (a, b) = rest.split_once("-B")?;
            return Some(SourceOffset::Bytes {
                start_byte: a.parse().ok()?,
                end_byte: b.parse().ok()?,
            });
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    pub timestamp: Option<String>,
    pub source_offset: SourceOffset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSession {
    pub agent: AgentKind,
    /// Globally unambiguous session identifier (`session_id` within an agent).
    pub session_id: String,
    /// Exact source trace path (for DB-backed stores, `<db_path>#<id>`).
    pub source_path: String,
    /// Working directory when recorded by the session header.
    pub cwd: Option<String>,
    /// Session creation timestamp (RFC3339 UTC) — immutable once recorded.
    pub started_at: Option<String>,
    /// Volatile latest timestamp — stored in catalog only, NEVER in mirror body.
    pub ended_at: Option<String>,
    /// Volatile display title — stored in catalog only, NEVER in mirror body.
    pub title: Option<String>,
    pub is_subagent: bool,
    pub parent_session_id: Option<String>,
    pub turns: Vec<Turn>,
}

impl ParsedSession {
    pub fn catalog_key(&self) -> String {
        format!("{}:{}", self.agent.as_str(), self.session_id)
    }

    /// Derive a display title for catalog/search output without putting volatile text in mirror files.
    pub fn display_title(&self) -> String {
        if let Some(t) = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return truncate_chars(collapse_ws(t), 120);
        }
        if let Some(first_user) = self
            .turns
            .iter()
            .find(|t| t.role == Role::User && !t.text.trim().is_empty())
        {
            return truncate_chars(collapse_ws(&first_user.text), 100);
        }
        self.session_id.clone()
    }
}

/// Discovered candidate source item prior to full parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCandidate {
    pub agent: AgentKind,
    /// Stage item key: relative-safe, unique across all agents, e.g. `codex/<hash>/<name>`.
    pub item_key: String,
    /// Version token that changes whenever the source session changes.
    pub version: String,
    /// Display/trace path (`path` or `db_path#id`).
    pub source_path: String,
    /// Underlying filesystem path used to open the source.
    pub fs_path: PathBuf,
    /// Optional inner id for multi-session SQLite stores.
    pub inner_id: Option<String>,
}

pub fn ms_to_rfc3339(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

pub fn sanitize_inline(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_control() || c == '|' || c == ']' || c == '[' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

pub fn collapse_ws(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate_chars(raw: String, max_chars: usize) -> String {
    if raw.chars().count() <= max_chars {
        return raw;
    }
    let mut out: String = raw.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn is_ephemeral_temp_cwd(cwd: &str) -> bool {
    let p = cwd.trim().trim_end_matches('/');
    p == "/tmp"
        || p == "/private/tmp"
        || p.starts_with("/tmp/")
        || p.starts_with("/private/tmp/")
        || p.starts_with("/var/folders/")
        || p.starts_with("/private/var/folders/")
        || is_autorag_workspace(p)
}

/// AutoRAG runs its own librarian agents in `<home>/.autorag/workspace` and its benchmark
/// harness in `<repo>/.bench/ws-*` (or `.bench-ws-*`); those sessions are machine-issued
/// prompts, not conversations with the user.
fn is_autorag_workspace(p: &str) -> bool {
    p.ends_with("/.autorag/workspace")
        || p.contains("/.autorag/workspace/")
        || p.contains("/.bench/ws-")
        || p.contains("/.bench-ws-")
}
