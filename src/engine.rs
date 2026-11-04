use crate::catalog::{Catalog, CatalogSession, CatalogTurn};
use crate::config::RuntimeConfig;
use crate::minsync::{MinSyncQueryHit, MinSyncSyncResult, QueryMode, StageItem, StageOpenParams};
use crate::mirror::{locate_chunk_turns, parse_mirror_document, render_mirror};
use crate::model::{AgentKind, SourceCandidate, SourceOffset};
use crate::parsers::{
    PARSER_FINGERPRINT, ParseOptions, ParseOutcome, discover_all, extract_raw_trace,
    parse_candidate,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub ready: bool,
    pub version: String,
    pub minsync_version: String,
    pub data_dir: String,
    pub mirror_dir: String,
    pub embedder: String,
    pub freshness: FreshnessSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreshnessSummary {
    pub indexed_sessions: usize,
    pub indexed_turns: usize,
    pub minsync_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub ok: bool,
    pub discovered: usize,
    pub processed: usize,
    pub written: usize,
    pub unchanged_mirror: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub failed: usize,
    pub total_sessions: usize,
    /// Alias matching AutoRAG lazykatok `total_messages` normalization.
    pub total_messages: usize,
    pub indexed: Option<IndexReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexReport {
    pub ok: bool,
    #[serde(rename = "chunkCount")]
    pub chunk_count: usize,
    pub candidate_chunks: usize,
    pub files_processed: usize,
    pub chunks_added: usize,
    pub chunks_updated: usize,
    pub chunks_deleted: usize,
    pub embedded_texts: usize,
    pub embeddings_reused: usize,
    pub sparse_encoded: usize,
    pub sparse_reused: usize,
    pub already_up_to_date: bool,
    pub query_ready: bool,
}

impl From<MinSyncSyncResult> for IndexReport {
    fn from(r: MinSyncSyncResult) -> Self {
        let active_chunks = r.chunks_added + r.chunks_updated;
        Self {
            ok: !r.partial,
            chunk_count: active_chunks,
            candidate_chunks: active_chunks,
            files_processed: r.files_processed,
            chunks_added: r.chunks_added,
            chunks_updated: r.chunks_updated,
            chunks_deleted: r.chunks_deleted,
            embedded_texts: r.embedded_texts,
            embeddings_reused: r.embeddings_reused,
            sparse_encoded: r.sparse_encoded,
            sparse_reused: r.sparse_reused,
            already_up_to_date: r.already_up_to_date,
            query_ready: r.query_ready,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub chunk_id: String,
    pub score: f64,
    pub snippet: String,
    pub content: String,
    pub agent: String,
    pub session_id: String,
    pub catalog_key: String,
    pub title: String,
    /// Alias consumed by AutoRAG chat datasource metadata (`chat_name`).
    pub chat_name: String,
    /// Alias consumed by AutoRAG chat datasource metadata (`sender_nickname`).
    pub sender_nickname: String,
    pub source_path: String,
    pub mirror_path: String,
    pub cwd: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub start_turn: usize,
    pub end_turn: usize,
    pub source_offsets: Vec<SourceOffset>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_rank: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInspect {
    pub chunk_id: String,
    pub text: String,
    pub content: String,
    pub parent_id: String,
    pub agent: String,
    pub session_id: String,
    pub source_path: String,
    pub mirror_path: String,
    pub start_turn: usize,
    pub end_turn: usize,
    pub source_offsets: Vec<SourceOffset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkContextResponse {
    pub chunk: ChunkInspect,
    pub previous: Vec<CatalogTurn>,
    pub next: Vec<CatalogTurn>,
    pub parent_windows: Vec<ParentWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParentWindow {
    pub parent_id: String,
    pub session_id: String,
    pub agent: String,
    pub source_path: String,
    pub title: String,
    pub cwd: Option<String>,
    pub text: String,
}

pub fn run_doctor(cfg: &RuntimeConfig) -> Result<DoctorReport> {
    let ms = cfg.minsync_config();
    let semver = ms.check_version()?;
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let (indexed_sessions, indexed_turns) = catalog.count_sessions_and_turns()?;
    let minsync_state = ms
        .status()
        .ok()
        .and_then(|v| {
            v.get("state")
                .and_then(|s| s.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "not_initialized".to_string());

    Ok(DoctorReport {
        ok: true,
        ready: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        minsync_version: semver.raw,
        data_dir: cfg.data_dir.to_string_lossy().into_owned(),
        mirror_dir: cfg.mirror_dir.to_string_lossy().into_owned(),
        embedder: cfg.embedder.clone(),
        freshness: FreshnessSummary {
            indexed_sessions,
            indexed_turns,
            minsync_state,
        },
    })
}

pub struct SyncOptions {
    pub agents: Option<Vec<AgentKind>>,
    pub full: bool,
    pub retry_failed: bool,
    pub also_index: bool,
}

pub fn run_sync(cfg: &RuntimeConfig, opts: &SyncOptions) -> Result<SyncReport> {
    std::fs::create_dir_all(&cfg.mirror_dir)?;
    let ms = cfg.minsync_config();
    ms.check_version()?;

    let parse_opts = ParseOptions {
        include_subagents: cfg.include_subagents,
        include_ephemeral: cfg.include_ephemeral,
    };
    let candidates = discover_all(&cfg.roots, opts.agents.as_deref(), &parse_opts)?;
    let by_key: HashMap<String, &SourceCandidate> =
        candidates.iter().map(|c| (c.item_key.clone(), c)).collect();

    let stage_items: Vec<StageItem> = candidates
        .iter()
        .map(|c| StageItem {
            key: c.item_key.clone(),
            version: c.version.clone(),
        })
        .collect();

    let mut catalog = Catalog::open(&cfg.catalog_path)?;
    let mut stage = ms.open_stage_session()?;

    // An `--agent` scoped run only discovers some agents; keys the stage already holds for the
    // others (including skipped ones, which the catalog never sees) must be preserved, not deleted.
    let unknown_preserved: Vec<String> = match opts.agents.as_deref() {
        Some(included) => {
            let scope_prefixes: Vec<String> = included
                .iter()
                .map(|a| format!("{}/", a.as_str()))
                .collect();
            stage
                .known_keys("wombat-sessions", &cfg.mirror_dir)?
                .into_iter()
                .filter(|k| !scope_prefixes.iter().any(|p| k.starts_with(p)))
                .collect()
        }
        None => Vec::new(),
    };

    let fingerprint = format!(
        "{PARSER_FINGERPRINT}:sub={}:eph={}",
        cfg.include_subagents, cfg.include_ephemeral
    );
    let open_resp = stage.open(&StageOpenParams {
        source_id: "wombat-sessions",
        fingerprint: &fingerprint,
        output_root: &cfg.mirror_dir,
        items: &stage_items,
        unknown: &unknown_preserved,
        full: opts.full,
        retry_failed: opts.retry_failed,
    })?;

    let mut processed = 0usize;
    let mut written = 0usize;
    let mut unchanged_mirror = 0usize;
    let mut skipped = open_resp.counts.skip;
    let mut deleted = 0usize;
    let mut failed = 0usize;

    for decision in &open_resp.decisions {
        match decision.action.as_str() {
            "process" | "retry" => {
                let Some(cand) = by_key.get(&decision.key) else {
                    continue;
                };
                processed += 1;
                match parse_candidate(cand, &parse_opts) {
                    Ok(ParseOutcome::Session(session)) => {
                        let rendered = render_mirror(&session);
                        let abs_out = cfg.mirror_dir.join(Path::new(&rendered.relative_path));
                        let prev = catalog.get_mirror_sha256(&cand.item_key)?;

                        let need_write = !matches!(
                            &prev,
                            Some((prev_rel, prev_sha))
                                if prev_rel == &rendered.relative_path
                                    && prev_sha == &rendered.sha256_hex
                                    && abs_out.is_file()
                        );

                        // If a session's relative path ever changed, unlink the stale file.
                        if let Some((prev_rel, _)) = &prev
                            && prev_rel != &rendered.relative_path
                        {
                            let old_abs = cfg.mirror_dir.join(Path::new(prev_rel));
                            let _ = std::fs::remove_file(old_abs);
                        }

                        if need_write {
                            if let Some(parent) = abs_out.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&abs_out, &rendered.content).with_context(|| {
                                format!("failed writing mirror {}", abs_out.display())
                            })?;
                            written += 1;
                        } else {
                            unchanged_mirror += 1;
                        }

                        catalog.upsert_session(
                            &cand.item_key,
                            &session,
                            &rendered.relative_path,
                            &rendered.sha256_hex,
                            &rendered.turn_spans,
                        )?;

                        stage.commit(
                            &cand.item_key,
                            &cand.version,
                            &rendered.relative_path,
                            rendered.content.len() as u64,
                            &rendered.sha256_hex,
                        )?;
                    }
                    Ok(ParseOutcome::Skipped { reason }) => {
                        let _ = catalog.remove_by_item_key(&cand.item_key)?;
                        stage.skip(&cand.item_key, &cand.version, reason)?;
                        skipped += 1;
                    }
                    Err(err) => {
                        failed += 1;
                        stage.fail(&cand.item_key, &cand.version, &format!("{err:#}"), true)?;
                    }
                }
            }
            "delete" => {
                let _ = stage.delete(&decision.key)?;
                let _ = catalog.remove_by_item_key(&decision.key)?;
                deleted += 1;
            }
            _ => {}
        }
    }

    let _ = stage.finish()?;
    let (total_sessions, total_turns) = catalog.count_sessions_and_turns()?;

    let indexed = if opts.also_index {
        Some(run_index(cfg, opts.full)?)
    } else {
        None
    };

    Ok(SyncReport {
        ok: failed == 0,
        discovered: candidates.len(),
        processed,
        written,
        unchanged_mirror,
        skipped,
        deleted,
        failed,
        total_sessions,
        total_messages: total_turns,
        indexed,
    })
}

pub fn run_index(cfg: &RuntimeConfig, full: bool) -> Result<IndexReport> {
    let ms = cfg.minsync_config();
    let sync_res = ms.sync(full, true)?;
    let mut report = IndexReport::from(sync_res);
    if report.chunk_count == 0 {
        // When already_up_to_date, report total turns as proxy non-zero chunk count for AutoRAG.
        if let Ok(cat) = Catalog::open(&cfg.catalog_path)
            && let Ok((_, turns)) = cat.count_sessions_and_turns()
        {
            report.chunk_count = turns;
            report.candidate_chunks = turns;
        }
    }
    Ok(report)
}

pub struct SearchParams<'a> {
    pub query: &'a str,
    pub mode: QueryMode,
    pub limit: usize,
    pub agent_filter: Option<AgentKind>,
    pub project_filter: Option<&'a str>,
}

pub fn run_search(cfg: &RuntimeConfig, params: &SearchParams<'_>) -> Result<Vec<SearchHit>> {
    if params.query.trim().is_empty() || params.limit == 0 {
        return Ok(Vec::new());
    }
    let ms = cfg.minsync_config();
    let catalog = Catalog::open(&cfg.catalog_path)?;

    let has_filter = params.agent_filter.is_some() || params.project_filter.is_some();
    let fetch_k = if has_filter {
        (params.limit * 6).clamp(30, 300)
    } else {
        params.limit
    };

    let raw_hits = ms.query(params.query, params.mode, fetch_k)?;
    let mut out = Vec::with_capacity(params.limit.min(raw_hits.len()));

    for hit in raw_hits {
        if let Some(enriched) = enrich_hit(cfg, &catalog, hit)? {
            if let Some(req_agent) = params.agent_filter
                && enriched.agent != req_agent.as_str()
            {
                continue;
            }
            if let Some(proj) = params.project_filter {
                let low = proj.to_ascii_lowercase();
                let matches_cwd = enriched
                    .cwd
                    .as_deref()
                    .is_some_and(|c| c.to_ascii_lowercase().contains(&low));
                let matches_mirror = enriched.mirror_path.to_ascii_lowercase().contains(&low);
                if !matches_cwd && !matches_mirror {
                    continue;
                }
            }
            out.push(enriched);
            if out.len() >= params.limit {
                break;
            }
        }
    }
    Ok(out)
}

fn enrich_hit(
    cfg: &RuntimeConfig,
    catalog: &Catalog,
    hit: MinSyncQueryHit,
) -> Result<Option<SearchHit>> {
    let mirror_abs = cfg.mirror_dir.join(Path::new(&hit.path));
    let mirror_content = std::fs::read_to_string(&mirror_abs).unwrap_or_default();

    let session_opt = catalog.get_by_mirror_path(&hit.path)?;
    let (catalog_key, agent_str, session_id, title, source_path, cwd, started_at, ended_at, spans) =
        if let Some(s) = session_opt {
            let spans = catalog.get_turn_spans(&s.catalog_key)?;
            (
                s.catalog_key,
                s.agent.as_str().to_string(),
                s.session_id,
                s.title,
                s.source_path,
                s.cwd,
                s.started_at,
                s.ended_at,
                spans,
            )
        } else if !mirror_content.is_empty() {
            let parsed = parse_mirror_document(&mirror_content);
            let a = parsed
                .agent
                .map(|k| k.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let sid = parsed
                .session_id
                .clone()
                .unwrap_or_else(|| hit.path.clone());
            let ck = format!("{a}:{sid}");
            (
                ck,
                a,
                sid.clone(),
                sid,
                parsed.source_path.unwrap_or_default(),
                parsed.cwd,
                parsed.started_at,
                None,
                parsed.turn_spans,
            )
        } else {
            return Ok(None);
        };

    let matched_spans = locate_chunk_turns(&mirror_content, &spans, &hit.text);
    let (start_turn, end_turn, source_offsets, turn_started, turn_ended) =
        if !matched_spans.is_empty() {
            let st = matched_spans.first().map(|s| s.turn_index).unwrap_or(1);
            let et = matched_spans.last().map(|s| s.turn_index).unwrap_or(st);
            let offsets: Vec<SourceOffset> = matched_spans
                .iter()
                .map(|s| s.source_offset.clone())
                .collect();
            let ts_first = matched_spans.iter().find_map(|s| s.timestamp.clone());
            let ts_last = matched_spans.iter().rev().find_map(|s| s.timestamp.clone());
            (st, et, offsets, ts_first, ts_last)
        } else {
            (1, spans.len().max(1), Vec::new(), None, None)
        };

    let _ = catalog.record_chunk(
        &hit.doc_id,
        &catalog_key,
        &hit.path,
        start_turn,
        end_turn,
        &hit.text,
    );

    let chat_name = format!("[{agent_str}] {title}");
    Ok(Some(SearchHit {
        chunk_id: hit.doc_id,
        score: hit.score,
        snippet: hit.text.clone(),
        content: hit.text,
        agent: agent_str.clone(),
        session_id,
        catalog_key,
        title,
        chat_name,
        sender_nickname: agent_str,
        source_path,
        mirror_path: hit.path,
        cwd,
        started_at: turn_started.or(started_at),
        ended_at: turn_ended.or(ended_at),
        start_turn,
        end_turn,
        source_offsets,
        mode: hit.mode,
        vector_rank: hit.vector_rank,
        sparse_rank: hit.sparse_rank,
    }))
}

pub fn run_chunk_get(cfg: &RuntimeConfig, chunk_id: &str) -> Result<ChunkInspect> {
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let recorded = catalog
        .get_chunk(chunk_id)?
        .ok_or_else(|| anyhow!("unknown chunk_id {chunk_id:?} (search first or re-run index)"))?;
    let session = catalog
        .resolve_session(&recorded.catalog_key)?
        .ok_or_else(|| anyhow!("session {} missing from catalog", recorded.catalog_key))?;
    let turns = catalog.get_turns(&session.catalog_key)?;
    let offsets: Vec<SourceOffset> = turns
        .iter()
        .filter(|t| t.turn_index >= recorded.start_turn && t.turn_index <= recorded.end_turn)
        .map(|t| t.source_offset.clone())
        .collect();

    Ok(ChunkInspect {
        chunk_id: recorded.chunk_id,
        text: recorded.text.clone(),
        content: recorded.text,
        parent_id: session.catalog_key.clone(),
        agent: session.agent.as_str().to_string(),
        session_id: session.session_id,
        source_path: session.source_path,
        mirror_path: session.mirror_rel_path,
        start_turn: recorded.start_turn,
        end_turn: recorded.end_turn,
        source_offsets: offsets,
    })
}

pub fn run_chunk_context(
    cfg: &RuntimeConfig,
    chunk_id: &str,
    window: usize,
) -> Result<ChunkContextResponse> {
    let chunk = run_chunk_get(cfg, chunk_id)?;
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let turns = catalog.get_turns(&chunk.parent_id)?;
    let prev_lo = chunk.start_turn.saturating_sub(window).max(1);
    let previous: Vec<CatalogTurn> = turns
        .iter()
        .filter(|t| t.turn_index >= prev_lo && t.turn_index < chunk.start_turn)
        .cloned()
        .collect();
    let next_hi = chunk.end_turn.saturating_add(window);
    let next: Vec<CatalogTurn> = turns
        .iter()
        .filter(|t| t.turn_index > chunk.end_turn && t.turn_index <= next_hi)
        .cloned()
        .collect();
    let parent_windows = run_chunk_parent(cfg, chunk_id)?;
    Ok(ChunkContextResponse {
        chunk,
        previous,
        next,
        parent_windows,
    })
}

pub fn run_chunk_parent(cfg: &RuntimeConfig, chunk_id: &str) -> Result<Vec<ParentWindow>> {
    let chunk = run_chunk_get(cfg, chunk_id)?;
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let session = catalog
        .resolve_session(&chunk.parent_id)?
        .ok_or_else(|| anyhow!("session {} not found", chunk.parent_id))?;
    let mirror_abs = cfg.mirror_dir.join(Path::new(&session.mirror_rel_path));
    let full_text = std::fs::read_to_string(&mirror_abs).unwrap_or_else(|_| chunk.text.clone());

    Ok(vec![ParentWindow {
        parent_id: session.catalog_key,
        session_id: session.session_id,
        agent: session.agent.as_str().to_string(),
        source_path: session.source_path,
        title: session.title,
        cwd: session.cwd,
        text: full_text,
    }])
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session: CatalogSession,
    pub turns: Vec<CatalogTurn>,
}

pub fn run_show_session(
    cfg: &RuntimeConfig,
    selector: &str,
    turn_filter: Option<usize>,
) -> Result<SessionDetail> {
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let session = catalog
        .resolve_session(selector)?
        .ok_or_else(|| anyhow!("session {selector:?} not found"))?;
    let mut turns = catalog.get_turns(&session.catalog_key)?;
    if let Some(t_idx) = turn_filter {
        turns.retain(|t| t.turn_index == t_idx);
    }
    Ok(SessionDetail { session, turns })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceDetail {
    pub catalog_key: String,
    pub agent: String,
    pub session_id: String,
    pub source_path: String,
    pub turn_index: Option<usize>,
    pub source_offset: Option<SourceOffset>,
    pub raw_trace: String,
}

pub fn run_trace(
    cfg: &RuntimeConfig,
    selector: &str,
    turn_index: Option<usize>,
    context_lines: u64,
) -> Result<TraceDetail> {
    let catalog = Catalog::open(&cfg.catalog_path)?;
    let session = catalog
        .resolve_session(selector)?
        .ok_or_else(|| anyhow!("session {selector:?} not found"))?;
    let turns = catalog.get_turns(&session.catalog_key)?;

    let (chosen_turn, offset_opt, raw_trace) = if let Some(t_idx) = turn_index {
        let turn = turns
            .iter()
            .find(|t| t.turn_index == t_idx)
            .ok_or_else(|| anyhow!("turn {t_idx} not found in {}", session.catalog_key))?;
        let raw = extract_raw_trace(
            session.agent,
            &session.source_path,
            &turn.source_offset,
            context_lines,
        )?;
        (Some(t_idx), Some(turn.source_offset.clone()), raw)
    } else if let (Some(first), Some(last)) = (turns.first(), turns.last()) {
        // Span across entire session if line-based, else concatenate turn items.
        match (&first.source_offset, &last.source_offset) {
            (SourceOffset::Lines { start_line, .. }, SourceOffset::Lines { end_line, .. }) => {
                let span = SourceOffset::Lines {
                    start_line: 1,
                    end_line: (*end_line).max(*start_line) + context_lines,
                };
                let raw = extract_raw_trace(session.agent, &session.source_path, &span, 0)?;
                (None, Some(span), raw)
            }
            _ => {
                let mut chunks = Vec::new();
                for t in &turns {
                    let piece = extract_raw_trace(
                        session.agent,
                        &session.source_path,
                        &t.source_offset,
                        context_lines,
                    )?;
                    chunks.push(format!("--- Turn {} ---\n{}", t.turn_index, piece));
                }
                (None, None, chunks.join("\n"))
            }
        }
    } else {
        (None, None, String::new())
    };

    Ok(TraceDetail {
        catalog_key: session.catalog_key,
        agent: session.agent.as_str().to_string(),
        session_id: session.session_id,
        source_path: session.source_path,
        turn_index: chosen_turn,
        source_offset: offset_opt,
        raw_trace,
    })
}
