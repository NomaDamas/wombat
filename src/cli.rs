use crate::catalog::Catalog;
use crate::config::{ConfigOverrides, RuntimeConfig, parse_source_arg};
use crate::engine::{
    SearchParams, SyncOptions, run_chunk_context, run_chunk_get, run_chunk_parent, run_doctor,
    run_index, run_search, run_show_session, run_sync, run_trace,
};
use crate::minsync::QueryMode;
use crate::model::{AgentKind, collapse_ws, truncate_chars};
use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "wombat",
    version,
    about = "Distill coding-agent sessions into a prefix-stable mirror indexed by MinSync"
)]
pub struct Cli {
    /// Data directory holding `mirror/`, `stage/`, and `catalog.sqlite3` (default: `~/.wombat`)
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Path to `config.toml`
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Path to external `minsync` binary (>= 0.6.0)
    #[arg(long, global = true)]
    pub minsync_bin: Option<String>,

    /// Dense embedder id passed to `minsync init` (`native:Qwen/Qwen3-Embedding-0.6B`, `none` for SPLADE-only)
    #[arg(long, global = true)]
    pub embedder: Option<String>,

    /// Chunker id passed to `minsync init` (`auto` -> `line-cdc` on 0.7+ else `cdc`)
    #[arg(long, global = true)]
    pub chunker: Option<String>,

    /// Explicit source root(s): `<path>` or `<agent>=<path>` (e.g. `codex=/path/to/sessions`)
    #[arg(long = "source", global = true)]
    pub sources: Vec<String>,

    /// Include subagent / sidechain transcripts (skipped by default)
    #[arg(long, global = true)]
    pub include_subagents: bool,

    /// Include ephemeral `/tmp` / `/var/folders` / `faux` sessions (skipped by default)
    #[arg(long, global = true)]
    pub include_ephemeral: bool,

    /// Emit machine-readable JSON on stdout
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Verify `minsync >= 0.6.0` health and inspect wombat index freshness
    Doctor,

    /// Incrementally parse coding-agent sessions into the prefix-stable mirror directory
    Sync(SyncArgs),

    /// Run `minsync sync` (SPLADE + dense incremental indexing) over the mirror directory
    Index(IndexArgs),

    /// Search indexed sessions (`wombat search [keyword|semantic|hybrid] <query>` or `--mode`)
    Search(SearchArgs),

    /// List mirrored coding-agent sessions
    Sessions(SessionsArgs),

    /// Show distilled user/assistant turns for a session (`<session_id>` or `<agent>:<session_id>`)
    Show(ShowArgs),

    /// Jump to the original raw session trace (including tool calls & thinking) for a session or turn
    Trace(TraceArgs),

    /// Inspect indexed chunks and their surrounding turn/session windows (AutoRAG datasource contract)
    Chunk {
        #[command(subcommand)]
        sub: ChunkSubcommand,
    },
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Restrict discovery to specific agent(s): codex, claude-code, opencode, pi, omp, hermes, aside, kilo, gemini, qwen
    #[arg(long = "agent")]
    pub agents: Vec<String>,

    /// Force full re-parse of all discovered sessions
    #[arg(long)]
    pub full: bool,

    /// Retry previously deferred parse failures
    #[arg(long)]
    pub retry_failed: bool,

    /// Also run `minsync sync` immediately after updating the mirror
    #[arg(long)]
    pub index: bool,
}

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Force a full MinSync re-index
    #[arg(long)]
    pub full: bool,

    /// Alias for `--full` (matches mailcrawl `index --rebuild` convention)
    #[arg(long)]
    pub rebuild: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Positional `[mode] <query>` or `<query>` (matches both `wombat search hybrid "q"` and `wombat search --mode hybrid "q"`)
    #[arg(required = true, num_args = 1..=2)]
    pub terms: Vec<String>,

    /// Explicit retrieval mode: `sparse`/`keyword`/`bm25`, `semantic`/`vector`, or `hybrid`
    #[arg(long, short = 'm')]
    pub mode: Option<String>,

    /// Maximum number of hits to return
    #[arg(long, short = 'k', visible_alias = "top-k", default_value_t = 10)]
    pub limit: usize,

    /// Filter hits to one coding agent (`codex`, `claude-code`, `opencode`, `pi`, `omp`, `hermes`, `aside`, `gemini`, `qwen`)
    #[arg(long)]
    pub agent: Option<String>,

    /// Filter hits whose session `cwd` or mirror path contains this substring
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
    #[arg(long)]
    pub agent: Option<String>,

    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Session selector (`<session_id>`, unique prefix, or `<agent>:<session_id>`)
    pub session: String,

    /// Show only a single 1-based turn
    #[arg(long)]
    pub turn: Option<usize>,
}

#[derive(Debug, Args)]
pub struct TraceArgs {
    /// Session selector (`<session_id>`, unique prefix, or `<agent>:<session_id>`)
    pub session: String,

    /// 1-based turn index to inspect in the raw session log
    #[arg(long)]
    pub turn: Option<usize>,

    /// Surrounding raw JSONL lines before/after the turn's offset
    #[arg(long, default_value_t = 5)]
    pub context: u64,
}

#[derive(Debug, Subcommand)]
pub enum ChunkSubcommand {
    /// Get one indexed chunk and its session/turn pointers
    Get { chunk_id: String },
    /// Get surrounding turns and parent session window for a chunk
    Context {
        chunk_id: String,
        #[arg(long, default_value_t = 2)]
        window: usize,
    },
    /// Get the full parent session mirror document for a chunk
    Parent { chunk_id: String },
}

pub fn run(cli: Cli) -> Result<()> {
    let source_overrides = cli
        .sources
        .iter()
        .map(|s| parse_source_arg(s))
        .collect::<Vec<_>>();
    let cfg = RuntimeConfig::load(ConfigOverrides {
        data_dir: cli.data_dir,
        config_path: cli.config,
        minsync_binary: cli.minsync_bin,
        embedder: cli.embedder,
        chunker: cli.chunker,
        include_subagents: cli.include_subagents,
        include_ephemeral: cli.include_ephemeral,
        source_overrides,
    })?;

    match cli.command {
        Commands::Doctor => {
            let report = run_doctor(&cfg)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("wombat {} (ready={})", report.version, report.ready);
                println!("  minsync:   {}", report.minsync_version);
                println!("  data_dir:  {}", report.data_dir);
                println!("  mirror:    {}", report.mirror_dir);
                println!("  embedder:  {}", report.embedder);
                println!(
                    "  sessions:  {} ({} turns, index={})",
                    report.freshness.indexed_sessions,
                    report.freshness.indexed_turns,
                    report.freshness.minsync_state
                );
            }
        }
        Commands::Sync(args) => {
            let agents = parse_agent_list(&args.agents)?;
            let report = run_sync(
                &cfg,
                &SyncOptions {
                    agents,
                    full: args.full,
                    retry_failed: args.retry_failed,
                    also_index: args.index,
                },
            )?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Synced mirror: discovered={} processed={} written={} unchanged={} skipped={} deleted={} failed={} (total: {} sessions, {} turns)",
                    report.discovered,
                    report.processed,
                    report.written,
                    report.unchanged_mirror,
                    report.skipped,
                    report.deleted,
                    report.failed,
                    report.total_sessions,
                    report.total_messages
                );
                if let Some(idx) = &report.indexed {
                    println!(
                        "Indexed MinSync: files={} chunks_added={} chunks_updated={} embedded={} reused={} sparse_encoded={} sparse_reused={}",
                        idx.files_processed,
                        idx.chunks_added,
                        idx.chunks_updated,
                        idx.embedded_texts,
                        idx.embeddings_reused,
                        idx.sparse_encoded,
                        idx.sparse_reused
                    );
                }
            }
        }
        Commands::Index(args) => {
            let report = run_index(&cfg, args.full || args.rebuild)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Indexed MinSync: chunks={} (added={}, updated={}, deleted={}) embedded={} (reused={}) sparse={} (reused={}) up_to_date={}",
                    report.chunk_count,
                    report.chunks_added,
                    report.chunks_updated,
                    report.chunks_deleted,
                    report.embedded_texts,
                    report.embeddings_reused,
                    report.sparse_encoded,
                    report.sparse_reused,
                    report.already_up_to_date
                );
            }
        }
        Commands::Search(args) => {
            let (mode, query) = resolve_search_mode_and_query(&args)?;
            let agent_filter = match &args.agent {
                Some(a) => Some(
                    AgentKind::parse_name(a)
                        .ok_or_else(|| anyhow::anyhow!("unknown agent {a:?}"))?,
                ),
                None => None,
            };
            let hits = run_search(
                &cfg,
                &SearchParams {
                    query: &query,
                    mode,
                    limit: args.limit,
                    agent_filter,
                    project_filter: args.project.as_deref(),
                },
            )?;
            if cli.json {
                // Bare JSON array for direct compatibility with AutoRAG's CLI datasource normalizers.
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("No matches found.");
            } else {
                for (i, h) in hits.iter().enumerate() {
                    let offsets_label = h
                        .source_offsets
                        .iter()
                        .map(|o| o.compact_label())
                        .collect::<Vec<_>>()
                        .join(",");
                    println!(
                        "{}. [{:.4}] {} ({}) turns {}..={} [src:{}]",
                        i + 1,
                        h.score,
                        h.catalog_key,
                        h.title,
                        h.start_turn,
                        h.end_turn,
                        offsets_label
                    );
                    println!("   source: {}", h.source_path);
                    println!(
                        "   trace:  wombat trace {} --turn {}",
                        h.catalog_key, h.start_turn
                    );
                    println!(
                        "   snippet: {}",
                        truncate_chars(collapse_ws(&h.snippet), 180)
                    );
                }
            }
        }
        Commands::Sessions(args) => {
            let agent = match &args.agent {
                Some(a) => Some(
                    AgentKind::parse_name(a)
                        .ok_or_else(|| anyhow::anyhow!("unknown agent {a:?}"))?,
                ),
                None => None,
            };
            let catalog = Catalog::open(&cfg.catalog_path)?;
            let sessions = catalog.list_sessions(agent, args.limit)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for s in sessions {
                    println!(
                        "{:<12} {:<38} turns={:<4} {} ({})",
                        s.agent.as_str(),
                        s.session_id,
                        s.turn_count,
                        s.title,
                        s.cwd.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        Commands::Show(args) => {
            let detail = run_show_session(&cfg, &args.session, args.turn)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&detail)?);
            } else {
                println!(
                    "# {} ({})\n- source_path: {}\n- mirror_path: {}\n- cwd: {}\n",
                    detail.session.catalog_key,
                    detail.session.title,
                    detail.session.source_path,
                    detail.session.mirror_rel_path,
                    detail.session.cwd.as_deref().unwrap_or("-")
                );
                for t in detail.turns {
                    println!(
                        "## Turn {} | {} | {} [src:{}]\n{}\n",
                        t.turn_index,
                        t.role.heading_label(),
                        t.timestamp.as_deref().unwrap_or("-"),
                        t.source_offset.compact_label(),
                        t.text
                    );
                }
            }
        }
        Commands::Trace(args) => {
            let trace = run_trace(&cfg, &args.session, args.turn, args.context)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&trace)?);
            } else {
                println!(
                    "# Raw Trace: {} (turn={:?})\n# source_path: {}\n{}",
                    trace.catalog_key, trace.turn_index, trace.source_path, trace.raw_trace
                );
            }
        }
        Commands::Chunk { sub } => match sub {
            ChunkSubcommand::Get { chunk_id } => {
                let res = run_chunk_get(&cfg, &chunk_id)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else {
                    println!(
                        "chunk={} session={}:{} turns={}..={}\nsource={}\n\n{}",
                        res.chunk_id,
                        res.agent,
                        res.session_id,
                        res.start_turn,
                        res.end_turn,
                        res.source_path,
                        res.text
                    );
                }
            }
            ChunkSubcommand::Context { chunk_id, window } => {
                let res = run_chunk_context(&cfg, &chunk_id, window)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else {
                    println!(
                        "Context for {} ({}, prev={}, next={}):\n{}",
                        res.chunk.chunk_id,
                        res.chunk.parent_id,
                        res.previous.len(),
                        res.next.len(),
                        res.chunk.text
                    );
                }
            }
            ChunkSubcommand::Parent { chunk_id } => {
                let res = run_chunk_parent(&cfg, &chunk_id)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else {
                    for w in res {
                        println!("# {} ({})\n{}", w.parent_id, w.source_path, w.text);
                    }
                }
            }
        },
    }

    Ok(())
}

fn parse_agent_list(raw: &[String]) -> Result<Option<Vec<AgentKind>>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for item in raw {
        for piece in item.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let Some(kind) = AgentKind::parse_name(piece) else {
                bail!("unknown coding agent {piece:?}");
            };
            out.push(kind);
        }
    }
    Ok(Some(out))
}

fn resolve_search_mode_and_query(args: &SearchArgs) -> Result<(QueryMode, String)> {
    if let Some(explicit) = &args.mode {
        let mode = QueryMode::parse(explicit)
            .ok_or_else(|| anyhow::anyhow!("invalid search mode {explicit:?}"))?;
        return Ok((mode, args.terms.join(" ")));
    }
    if args.terms.len() == 2
        && let Some(parsed_mode) = QueryMode::parse(&args.terms[0])
    {
        return Ok((parsed_mode, args.terms[1].clone()));
    }
    Ok((QueryMode::Hybrid, args.terms.join(" ")))
}
