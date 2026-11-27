use crate::catalog::Catalog;
use crate::config::RuntimeConfig;
use crate::mirror::{MirrorTurnSpan, parse_mirror_document};
use crate::model::{AgentKind, SourceOffset};
use anyhow::{Context, Result, anyhow};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

const DEFAULT_MAX_PER_FILE: usize = 20;
const MAX_CONTEXT_LINES: usize = 20;

#[derive(Debug, Clone)]
pub struct GrepParams<'a> {
    pub pattern: &'a str,
    pub literal: bool,
    pub ignore_case: bool,
    pub context: usize,
    pub limit: usize,
    pub max_per_file: usize,
    pub agent_filter: Option<AgentKind>,
    pub project_filter: Option<&'a str>,
}

impl<'a> GrepParams<'a> {
    pub fn new(pattern: &'a str) -> Self {
        Self {
            pattern,
            literal: false,
            ignore_case: false,
            context: 0,
            limit: 50,
            max_per_file: DEFAULT_MAX_PER_FILE,
            agent_filter: None,
            project_filter: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepHit {
    pub chunk_id: String,
    pub score: f64,
    pub line_number: usize,
    pub line_end: usize,
    pub line: String,
    pub snippet: String,
    pub content: String,
    pub agent: String,
    pub session_id: String,
    pub catalog_key: String,
    pub title: String,
    pub chat_name: String,
    pub sender_nickname: String,
    pub source_path: String,
    pub mirror_path: String,
    pub cwd: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub turn_index: usize,
    pub start_turn: usize,
    pub end_turn: usize,
    pub source_offsets: Vec<SourceOffset>,
    pub mode: String,
}

#[derive(Debug, Clone)]
struct RawMatch {
    rel_path: String,
    line_number: usize,
    line_text: String,
}

pub fn run_grep(cfg: &RuntimeConfig, params: &GrepParams<'_>) -> Result<Vec<GrepHit>> {
    if params.pattern.is_empty() || params.limit == 0 {
        return Ok(Vec::new());
    }
    if !cfg.mirror_dir.is_dir() {
        return Ok(Vec::new());
    }

    let search_root = match params.agent_filter {
        Some(agent) => {
            let sub = cfg.mirror_dir.join(agent.as_str());
            if !sub.is_dir() {
                return Ok(Vec::new());
            }
            sub
        }
        None => cfg.mirror_dir.clone(),
    };

    let context_lines = params.context.min(MAX_CONTEXT_LINES);
    let max_per_file = params.max_per_file.max(1);
    let has_project_filter = params.project_filter.is_some_and(|p| !p.trim().is_empty());
    let collect_cap = if has_project_filter {
        params.limit.saturating_mul(10).clamp(50, 2000)
    } else {
        params.limit
    };

    let raw_matches = match resolve_rg_binary() {
        Some(rg) => run_rg_grep(
            &rg,
            &cfg.mirror_dir,
            &search_root,
            params,
            max_per_file,
            collect_cap,
        )?,
        None => run_rust_grep(
            &cfg.mirror_dir,
            &search_root,
            params,
            max_per_file,
            collect_cap,
        )?,
    };

    if raw_matches.is_empty() {
        return Ok(Vec::new());
    }

    let catalog = Catalog::open(&cfg.catalog_path)?;
    let mut file_cache: HashMap<String, Option<SessionFileInfo>> = HashMap::new();
    let mut out = Vec::with_capacity(params.limit.min(raw_matches.len()));

    for raw in raw_matches {
        let info = file_cache
            .entry(raw.rel_path.clone())
            .or_insert_with(|| load_session_info(&cfg.mirror_dir, &catalog, &raw.rel_path));
        let Some(info) = info.as_ref() else {
            continue;
        };

        if let Some(req_agent) = params.agent_filter
            && info.agent != req_agent.as_str()
        {
            continue;
        }
        if let Some(proj) = params.project_filter.filter(|p| !p.trim().is_empty()) {
            let low = proj.to_ascii_lowercase();
            let matches_cwd = info
                .cwd
                .as_deref()
                .is_some_and(|c| c.to_ascii_lowercase().contains(&low));
            let matches_mirror = info.mirror_rel_path.to_ascii_lowercase().contains(&low);
            if !matches_cwd && !matches_mirror {
                continue;
            }
        }

        let (line_start, line_end, snippet) =
            extract_context(&info.lines, raw.line_number, context_lines);
        let span = locate_turn_by_line(&info.spans, raw.line_number);
        let (turn_index, source_offsets, turn_ts) = match span {
            Some(s) => (
                s.turn_index,
                vec![s.source_offset.clone()],
                s.timestamp.clone(),
            ),
            None => (1, Vec::new(), None),
        };

        let rank = out.len() + 1;
        let score = 1.0 / (rank as f64);
        let chunk_id = format!("grep:{}:L{}", info.catalog_key, raw.line_number);
        let chat_name = format!("[{}] {}", info.agent, info.title);

        out.push(GrepHit {
            chunk_id,
            score,
            line_number: raw.line_number,
            line_end,
            line: raw.line_text,
            snippet: snippet.clone(),
            content: snippet,
            agent: info.agent.clone(),
            session_id: info.session_id.clone(),
            catalog_key: info.catalog_key.clone(),
            title: info.title.clone(),
            chat_name,
            sender_nickname: info.agent.clone(),
            source_path: info.source_path.clone(),
            mirror_path: info.mirror_rel_path.clone(),
            cwd: info.cwd.clone(),
            started_at: turn_ts.clone().or_else(|| info.started_at.clone()),
            ended_at: turn_ts.or_else(|| info.ended_at.clone()),
            turn_index,
            start_turn: turn_index,
            end_turn: turn_index,
            source_offsets,
            mode: "grep".to_string(),
        });

        let _ = line_start;
        if out.len() >= params.limit {
            break;
        }
    }

    Ok(out)
}

struct SessionFileInfo {
    catalog_key: String,
    agent: String,
    session_id: String,
    title: String,
    source_path: String,
    mirror_rel_path: String,
    cwd: Option<String>,
    started_at: Option<String>,
    ended_at: Option<String>,
    spans: Vec<MirrorTurnSpan>,
    lines: Vec<String>,
}

fn load_session_info(
    mirror_dir: &Path,
    catalog: &Catalog,
    rel_path: &str,
) -> Option<SessionFileInfo> {
    let abs = mirror_dir.join(Path::new(rel_path));
    let content = std::fs::read_to_string(&abs).ok()?;
    let lines: Vec<String> = content.lines().map(ToString::to_string).collect();

    if let Ok(Some(s)) = catalog.get_by_mirror_path(rel_path) {
        let spans = catalog.get_turn_spans(&s.catalog_key).unwrap_or_default();
        return Some(SessionFileInfo {
            catalog_key: s.catalog_key,
            agent: s.agent.as_str().to_string(),
            session_id: s.session_id,
            title: s.title,
            source_path: s.source_path,
            mirror_rel_path: rel_path.to_string(),
            cwd: s.cwd,
            started_at: s.started_at,
            ended_at: s.ended_at,
            spans,
            lines,
        });
    }

    let parsed = parse_mirror_document(&content);
    let agent = parsed
        .agent
        .map(|k| k.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let session_id = parsed
        .session_id
        .clone()
        .unwrap_or_else(|| rel_path.to_string());
    let catalog_key = format!("{agent}:{session_id}");
    Some(SessionFileInfo {
        catalog_key,
        agent,
        session_id: session_id.clone(),
        title: session_id,
        source_path: parsed.source_path.unwrap_or_default(),
        mirror_rel_path: rel_path.to_string(),
        cwd: parsed.cwd,
        started_at: parsed.started_at,
        ended_at: None,
        spans: parsed.turn_spans,
        lines,
    })
}

fn extract_context(lines: &[String], line_number: usize, context: usize) -> (usize, usize, String) {
    if lines.is_empty() || line_number == 0 {
        return (line_number, line_number, String::new());
    }
    let idx = line_number
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    let start_idx = idx.saturating_sub(context);
    let end_idx = (idx + context).min(lines.len().saturating_sub(1));
    let body = lines[start_idx..=end_idx].join("\n");
    (start_idx + 1, end_idx + 1, body)
}

fn locate_turn_by_line(spans: &[MirrorTurnSpan], line_number: usize) -> Option<&MirrorTurnSpan> {
    if let Some(exact) = spans
        .iter()
        .find(|s| line_number >= s.mirror_start_line && line_number <= s.mirror_end_line)
    {
        return Some(exact);
    }
    // Header lines before Turn 1 fall back to Turn 1; lines past the last span map to the last span.
    spans
        .iter()
        .rev()
        .find(|s| s.mirror_start_line <= line_number)
        .or_else(|| spans.first())
}

fn resolve_rg_binary() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["rg.exe", "rg"]
    } else {
        &["rg"]
    };
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<RgEventData>,
}

#[derive(Deserialize)]
struct RgEventData {
    path: Option<RgText>,
    line_number: Option<usize>,
    lines: Option<RgText>,
}

#[derive(Deserialize)]
struct RgText {
    text: Option<String>,
}

fn run_rg_grep(
    rg: &Path,
    mirror_dir: &Path,
    search_root: &Path,
    params: &GrepParams<'_>,
    max_per_file: usize,
    collect_cap: usize,
) -> Result<Vec<RawMatch>> {
    let mut cmd = Command::new(rg);
    cmd.current_dir(mirror_dir)
        .args([
            "--no-config",
            "--json",
            "--line-number",
            "--color=never",
            "--no-ignore",
            "--glob",
            "!/.minsync/**",
            "--glob",
            "!.minsync/**",
            "--glob",
            "*.md",
            "--max-count",
            &max_per_file.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if params.ignore_case {
        cmd.arg("--ignore-case");
    }
    if params.literal {
        cmd.arg("--fixed-strings");
    }

    let rel_target = search_root
        .strip_prefix(mirror_dir)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| ".".to_string());

    cmd.arg("--").arg(params.pattern).arg(&rel_target);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn ripgrep ({})", rg.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture ripgrep stdout"))?;
    let reader = BufReader::new(stdout);

    let mut grouped: BTreeMap<String, Vec<RawMatch>> = BTreeMap::new();
    let mut total = 0usize;
    let mut stopped_early = false;

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<RgEvent>(&line) else {
            continue;
        };
        if ev.kind != "match" {
            continue;
        }
        let Some(data) = ev.data else {
            continue;
        };
        let Some(raw_path) = data.path.and_then(|p| p.text) else {
            continue;
        };
        let Some(line_number) = data.line_number else {
            continue;
        };
        let norm_rel = raw_path
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
        if norm_rel.starts_with(".minsync/") || norm_rel.contains("/.minsync/") {
            continue;
        }
        let line_text = data
            .lines
            .and_then(|l| l.text)
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        grouped.entry(norm_rel.clone()).or_default().push(RawMatch {
            rel_path: norm_rel,
            line_number,
            line_text,
        });
        total += 1;
        if total >= collect_cap {
            stopped_early = true;
            let _ = child.kill();
            break;
        }
    }

    let output = child.wait_with_output()?;
    if !stopped_early && !matches!(output.status.code(), Some(0) | Some(1)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "ripgrep failed (code={:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    let mut out = Vec::with_capacity(total);
    for (_, mut list) in grouped {
        list.sort_by_key(|m| m.line_number);
        out.extend(list);
    }
    Ok(out)
}

fn compile_pattern(params: &GrepParams<'_>) -> Result<Regex> {
    let source = if params.literal {
        regex::escape(params.pattern)
    } else {
        params.pattern.to_string()
    };
    RegexBuilder::new(&source)
        .case_insensitive(params.ignore_case)
        .build()
        .with_context(|| format!("invalid grep pattern: {:?}", params.pattern))
}

fn run_rust_grep(
    mirror_dir: &Path,
    search_root: &Path,
    params: &GrepParams<'_>,
    max_per_file: usize,
    collect_cap: usize,
) -> Result<Vec<RawMatch>> {
    let re = compile_pattern(params)?;
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for entry in WalkDir::new(search_root)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".minsync")
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(mirror_dir) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            files.push((rel_str, entry.into_path()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut matches = Vec::new();
    for (rel_path, abs_path) in files {
        let Ok(content) = std::fs::read_to_string(&abs_path) else {
            continue;
        };
        let mut per_file = 0usize;
        for (idx, line) in content.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            matches.push(RawMatch {
                rel_path: rel_path.clone(),
                line_number: idx + 1,
                line_text: line.to_string(),
            });
            per_file += 1;
            if matches.len() >= collect_cap {
                return Ok(matches);
            }
            if per_file >= max_per_file {
                break;
            }
        }
    }
    Ok(matches)
}
