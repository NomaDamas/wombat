use crate::model::AgentKind;
use crate::parsers::SourceRoots;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_EMBEDDER: &str = "native:Qwen/Qwen3-Embedding-0.6B";
pub const DEFAULT_CHUNKER: &str = "auto";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileConfig {
    pub data_dir: Option<PathBuf>,
    pub minsync_binary: Option<String>,
    pub embedder: Option<String>,
    pub embed_base_url: Option<String>,
    pub chunker: Option<String>,
    pub max_chunk_size: Option<usize>,
    #[serde(default)]
    pub include_subagents: bool,
    #[serde(default)]
    pub include_ephemeral: bool,
    #[serde(default)]
    pub sources: SourcesConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesConfig {
    #[serde(default)]
    pub codex: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub claude_code: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub opencode: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub pi: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub omp: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub hermes: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub aside: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub kilo: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub gemini: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub data_dir: PathBuf,
    pub mirror_dir: PathBuf,
    pub stage_state_dir: PathBuf,
    pub catalog_path: PathBuf,
    pub minsync_binary: String,
    pub embedder: String,
    pub embed_base_url: Option<String>,
    pub chunker: String,
    pub max_chunk_size: Option<usize>,
    pub include_subagents: bool,
    pub include_ephemeral: bool,
    pub roots: SourceRoots,
}

pub struct ConfigOverrides {
    pub data_dir: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub minsync_binary: Option<String>,
    pub embedder: Option<String>,
    pub chunker: Option<String>,
    pub include_subagents: bool,
    pub include_ephemeral: bool,
    pub source_overrides: Vec<(Option<AgentKind>, PathBuf)>,
}

impl RuntimeConfig {
    pub fn load(overrides: ConfigOverrides) -> Result<Self> {
        let file_cfg = if let Some(cfg_path) = &overrides.config_path {
            let raw = std::fs::read_to_string(cfg_path)
                .with_context(|| format!("failed to read config {}", cfg_path.display()))?;
            toml::from_str::<FileConfig>(&raw)
                .with_context(|| format!("invalid TOML in {}", cfg_path.display()))?
        } else if let Some(default_cfg) = default_config_file()
            && default_cfg.is_file()
        {
            let raw = std::fs::read_to_string(&default_cfg)?;
            toml::from_str::<FileConfig>(&raw)?
        } else {
            FileConfig::default()
        };

        let data_dir = overrides
            .data_dir
            .or_else(|| std::env::var_os("WOMBAT_DATA_DIR").map(PathBuf::from))
            .or(file_cfg.data_dir)
            .unwrap_or_else(default_data_dir);

        let mirror_dir = data_dir.join("mirror");
        let stage_state_dir = data_dir.join("stage");
        let catalog_path = data_dir.join("catalog.sqlite3");

        let minsync_binary = overrides
            .minsync_binary
            .or_else(|| non_empty_env("WOMBAT_MINSYNC_BIN"))
            .or(file_cfg.minsync_binary)
            .unwrap_or_else(|| "minsync".to_string());

        let embedder = overrides
            .embedder
            .or_else(|| non_empty_env("WOMBAT_EMBEDDER"))
            .or(file_cfg.embedder)
            .unwrap_or_else(|| DEFAULT_EMBEDDER.to_string());

        let embed_base_url = non_empty_env("WOMBAT_EMBED_BASE_URL").or(file_cfg.embed_base_url);

        let chunker = overrides
            .chunker
            .or_else(|| non_empty_env("WOMBAT_CHUNKER"))
            .or(file_cfg.chunker)
            .unwrap_or_else(|| DEFAULT_CHUNKER.to_string());

        let include_subagents = overrides.include_subagents || file_cfg.include_subagents;
        let include_ephemeral = overrides.include_ephemeral || file_cfg.include_ephemeral;

        let mut roots = if !overrides.source_overrides.is_empty() {
            let mut explicit = SourceRoots::default();
            for (agent_opt, path) in overrides.source_overrides {
                match agent_opt {
                    Some(AgentKind::Codex) => explicit.codex.push(path),
                    Some(AgentKind::ClaudeCode) => explicit.claude_code.push(path),
                    Some(AgentKind::OpenCode) => explicit.opencode.push(path),
                    Some(AgentKind::Pi) => explicit.pi.push(path),
                    Some(AgentKind::Omp) => explicit.omp.push(path),
                    Some(AgentKind::Hermes) => explicit.hermes.push(path),
                    Some(AgentKind::Aside) => explicit.aside.push(path),
                    Some(AgentKind::Kilo) => explicit.kilo.push(path),
                    Some(AgentKind::Gemini) => explicit.gemini.push(path),
                    None => attach_auto_detected_source(&mut explicit, &path),
                }
            }
            explicit
        } else {
            let mut base = SourceRoots::default_from_env();
            if let Some(v) = file_cfg.sources.codex {
                base.codex = v;
            }
            if let Some(v) = file_cfg.sources.claude_code {
                base.claude_code = v;
            }
            if let Some(v) = file_cfg.sources.opencode {
                base.opencode = v;
            }
            if let Some(v) = file_cfg.sources.pi {
                base.pi = v;
            }
            if let Some(v) = file_cfg.sources.omp {
                base.omp = v;
            }
            if let Some(v) = file_cfg.sources.hermes {
                base.hermes = v;
            }
            if let Some(v) = file_cfg.sources.aside {
                base.aside = v;
            }
            if let Some(v) = file_cfg.sources.kilo {
                base.kilo = v;
            }
            if let Some(v) = file_cfg.sources.gemini {
                base.gemini = v;
            }
            base
        };

        // Ensure deterministic ordering of roots.
        roots.codex.sort();
        roots.claude_code.sort();
        roots.opencode.sort();
        roots.pi.sort();
        roots.omp.sort();
        roots.hermes.sort();
        roots.aside.sort();
        roots.kilo.sort();
        roots.gemini.sort();

        Ok(Self {
            data_dir,
            mirror_dir,
            stage_state_dir,
            catalog_path,
            minsync_binary,
            embedder,
            embed_base_url,
            chunker,
            max_chunk_size: file_cfg.max_chunk_size,
            include_subagents,
            include_ephemeral,
            roots,
        })
    }

    pub fn minsync_config(&self) -> crate::minsync::MinSyncConfig {
        crate::minsync::MinSyncConfig {
            binary: self.minsync_binary.clone(),
            mirror_dir: self.mirror_dir.clone(),
            stage_state_dir: self.stage_state_dir.clone(),
            embedder: self.embedder.clone(),
            chunker: self.chunker.clone(),
            max_chunk_size: self.max_chunk_size,
            embed_base_url: self.embed_base_url.clone(),
        }
    }
}

fn attach_auto_detected_source(roots: &mut SourceRoots, path: &Path) {
    let s = path.to_string_lossy().to_ascii_lowercase();
    if s.contains("codex") || s.ends_with(".zst") {
        roots.codex.push(path.to_path_buf());
    } else if s.contains("claude") {
        roots.claude_code.push(path.to_path_buf());
    } else if s.contains("opencode") || s.ends_with("opencode.db") {
        roots.opencode.push(path.to_path_buf());
    } else if s.contains("gemini") {
        roots.gemini.push(path.to_path_buf());
    } else if s.contains("kilo") {
        roots.kilo.push(path.to_path_buf());
    } else if s.contains("aside") {
        roots.aside.push(path.to_path_buf());
    } else if s.contains("hermes") {
        roots.hermes.push(path.to_path_buf());
    } else if s.contains(".omp") || s.contains("/omp") {
        roots.omp.push(path.to_path_buf());
    } else if s.contains(".pi") || s.contains("/pi") {
        roots.pi.push(path.to_path_buf());
    } else if path.join("storage").is_dir() {
        roots.opencode.push(path.to_path_buf());
    } else {
        roots.codex.push(path.to_path_buf());
        roots.claude_code.push(path.to_path_buf());
        roots.pi.push(path.to_path_buf());
    }
}

pub fn parse_source_arg(raw: &str) -> (Option<AgentKind>, PathBuf) {
    if let Some((prefix, rest)) = raw.split_once('=')
        && let Some(agent) = AgentKind::parse_name(prefix)
    {
        return (Some(agent), PathBuf::from(rest));
    }
    (None, PathBuf::from(raw))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".wombat"))
        .unwrap_or_else(|| PathBuf::from(".wombat"))
}

fn default_config_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WOMBAT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("wombat")
            .join("config.toml")
    })
}
