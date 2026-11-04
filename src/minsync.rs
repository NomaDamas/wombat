use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const MIN_REQUIRED_VERSION: (u32, u32, u32) = (0, 6, 0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemVer {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub raw: String,
}

impl SemVer {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let token = trimmed
            .strip_prefix("minsync")
            .unwrap_or(trimmed)
            .trim()
            .trim_start_matches('v');
        let core = token.split(['-', '+', ' ']).next().unwrap_or("");
        let mut parts = core.split('.');
        let major: u32 = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| anyhow!("invalid minsync version output: {trimmed:?}"))?;
        let minor: u32 = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| anyhow!("invalid minsync version output: {trimmed:?}"))?;
        let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Ok(Self {
            major,
            minor,
            patch,
            raw: trimmed.to_string(),
        })
    }

    pub fn meets_minimum(&self) -> bool {
        (self.major, self.minor, self.patch) >= MIN_REQUIRED_VERSION
    }
}

#[derive(Debug, Clone)]
pub struct MinSyncConfig {
    pub binary: String,
    pub mirror_dir: PathBuf,
    pub stage_state_dir: PathBuf,
    pub embedder: String,
    pub chunker: String,
    pub max_chunk_size: Option<usize>,
    pub embed_base_url: Option<String>,
}

impl MinSyncConfig {
    pub fn check_version(&self) -> Result<SemVer> {
        let output = Command::new(&self.binary)
            .arg("--version")
            .output()
            .with_context(|| {
                format!(
                    "failed to invoke minsync binary {:?} (install minsync >= 0.6.0)",
                    self.binary
                )
            })?;
        if !output.status.success() {
            bail!(
                "minsync --version exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let ver = SemVer::parse(&stdout)?;
        if !ver.meets_minimum() {
            bail!(
                "minsync >= 0.6.0 is required, found {} ({})",
                ver.raw,
                self.binary
            );
        }
        Ok(ver)
    }

    pub fn ensure_initialized(&self) -> Result<bool> {
        std::fs::create_dir_all(&self.mirror_dir).with_context(|| {
            format!("failed to create mirror dir {}", self.mirror_dir.display())
        })?;
        let config_path = self.mirror_dir.join(".minsync").join("config.toml");
        if config_path.is_file() {
            self.patch_config_if_needed(&config_path)?;
            return Ok(false);
        }

        let ver = self.check_version()?;
        // minsync 0.7+ provides `line-cdc`; 0.6.0 provides `cdc` (FastCDC).
        let effective_chunker = if self.chunker == "auto" {
            if (ver.major, ver.minor) >= (0, 7) {
                "line-cdc"
            } else {
                "cdc"
            }
        } else {
            self.chunker.as_str()
        };

        let output = Command::new(&self.binary)
            .current_dir(&self.mirror_dir)
            .args([
                "init",
                "--embedder",
                &self.embedder,
                "--chunker",
                effective_chunker,
                "--format",
                "json",
                "--quiet",
            ])
            .output()
            .context("failed to run `minsync init`")?;
        if !output.status.success() {
            bail!(
                "minsync init failed ({}): {}",
                output.status,
                stderr_or_stdout(&output)
            );
        }
        self.patch_config_if_needed(&config_path)?;
        Ok(true)
    }

    fn patch_config_if_needed(&self, config_path: &Path) -> Result<()> {
        let Some(raw) = std::fs::read_to_string(config_path).ok() else {
            return Ok(());
        };
        let mut doc: toml::Value = match toml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let mut changed = false;

        if let Some(max_chunk) = self.max_chunk_size
            && let Some(chunker_tbl) = doc.get_mut("chunker").and_then(toml::Value::as_table_mut)
        {
            let opts = chunker_tbl
                .entry("options")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let Some(opts_tbl) = opts.as_table_mut() {
                let target = toml::Value::Integer(max_chunk as i64);
                if opts_tbl.get("max_chunk_size") != Some(&target) {
                    opts_tbl.insert("max_chunk_size".to_string(), target);
                    changed = true;
                }
            }
        }

        if let Some(base_url) = self.embed_base_url.as_deref()
            && let Some(embed_tbl) = doc.get_mut("embedder").and_then(toml::Value::as_table_mut)
        {
            let target = toml::Value::String(base_url.to_string());
            if embed_tbl.get("base_url") != Some(&target) {
                embed_tbl.insert("base_url".to_string(), target);
                changed = true;
            }
        }

        if changed {
            let serialized = toml::to_string_pretty(&doc)?;
            std::fs::write(config_path, serialized)?;
        }
        Ok(())
    }

    pub fn sync(&self, full: bool, wait: bool) -> Result<MinSyncSyncResult> {
        self.ensure_initialized()?;
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.mirror_dir)
            .args(["sync", "--format", "json", "--quiet"]);
        if full {
            cmd.arg("--full");
        }
        if wait {
            cmd.arg("--wait");
        }
        let output = cmd.output().context("failed to run `minsync sync`")?;
        // Exit code 6 = partial sync in minsync; parse JSON if present before failing hard.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            bail!(
                "minsync sync failed ({}): {}",
                output.status,
                stderr_or_stdout(&output)
            );
        }
        let last_non_empty = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(stdout.trim());
        let parsed: MinSyncSyncResult = serde_json::from_str(if stdout.trim().starts_with('{') {
            stdout.trim()
        } else {
            last_non_empty
        })
        .with_context(|| format!("failed to parse `minsync sync` JSON: {stdout}"))?;
        Ok(parsed)
    }

    pub fn query(&self, text: &str, mode: QueryMode, top_k: usize) -> Result<Vec<MinSyncQueryHit>> {
        if text.trim().is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let effective_mode = self.resolve_query_mode(mode);
        let output = Command::new(&self.binary)
            .current_dir(&self.mirror_dir)
            .args([
                "query",
                "--format",
                "json",
                "--quiet",
                "-k",
                &top_k.to_string(),
                "--mode",
                effective_mode,
                text,
            ])
            .output()
            .context("failed to run `minsync query`")?;
        if !output.status.success() {
            bail!(
                "minsync query failed ({}): {}",
                output.status,
                stderr_or_stdout(&output)
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let hits: Vec<MinSyncQueryHit> = serde_json::from_str(&stdout)
            .with_context(|| format!("failed to parse `minsync query` JSON: {stdout}"))?;
        Ok(hits)
    }

    pub fn status(&self) -> Result<Value> {
        let output = Command::new(&self.binary)
            .current_dir(&self.mirror_dir)
            .args(["status", "--format", "json", "--quiet"])
            .output()
            .context("failed to run `minsync status`")?;
        if !output.status.success() {
            bail!(
                "minsync status failed ({}): {}",
                output.status,
                stderr_or_stdout(&output)
            );
        }
        let v: Value = serde_json::from_slice(&output.stdout)?;
        Ok(v)
    }

    fn resolve_query_mode(&self, requested: QueryMode) -> &'static str {
        let is_sparse_only = self.is_sparse_only_workspace();
        match (requested, is_sparse_only) {
            (QueryMode::Sparse, _) => "sparse",
            (_, true) => "sparse",
            (QueryMode::Vector, false) => "vector",
            (QueryMode::Hybrid, false) => "hybrid",
        }
    }

    pub fn is_sparse_only_workspace(&self) -> bool {
        if self.embedder.trim().eq_ignore_ascii_case("none") {
            return true;
        }
        let cfg_path = self.mirror_dir.join(".minsync").join("config.toml");
        if let Ok(raw) = std::fs::read_to_string(cfg_path)
            && let Ok(doc) = toml::from_str::<toml::Value>(&raw)
            && let Some(id) = doc
                .get("embedder")
                .and_then(|e| e.get("id"))
                .and_then(|v| v.as_str())
        {
            return id.trim().eq_ignore_ascii_case("none");
        }
        false
    }

    pub fn open_stage_session(&self) -> Result<StageSession> {
        std::fs::create_dir_all(&self.stage_state_dir).with_context(|| {
            format!(
                "failed to create stage state dir {}",
                self.stage_state_dir.display()
            )
        })?;
        std::fs::create_dir_all(&self.mirror_dir)?;
        StageSession::spawn(&self.binary, &self.stage_state_dir)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Sparse,
    Vector,
    Hybrid,
}

impl QueryMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "sparse" | "splade" | "keyword" | "bm25" | "lexical" => Some(QueryMode::Sparse),
            "vector" | "semantic" | "dense" => Some(QueryMode::Vector),
            "hybrid" => Some(QueryMode::Hybrid),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            QueryMode::Sparse => "sparse",
            QueryMode::Vector => "vector",
            QueryMode::Hybrid => "hybrid",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MinSyncSyncResult {
    #[serde(default)]
    pub files_processed: usize,
    #[serde(default)]
    pub chunks_added: usize,
    #[serde(default)]
    pub chunks_updated: usize,
    #[serde(default)]
    pub chunks_deleted: usize,
    #[serde(default)]
    pub files_added: usize,
    #[serde(default)]
    pub files_modified: usize,
    #[serde(default)]
    pub files_deleted: usize,
    #[serde(default)]
    pub already_up_to_date: bool,
    #[serde(default)]
    pub embedded_texts: usize,
    #[serde(default)]
    pub embeddings_reused: usize,
    #[serde(default)]
    pub sparse_encoded: usize,
    #[serde(default)]
    pub sparse_reused: usize,
    #[serde(default)]
    pub query_ready: bool,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinSyncQueryHit {
    pub doc_id: String,
    pub path: String,
    #[serde(default)]
    pub heading_path: String,
    #[serde(default)]
    pub chunk_type: String,
    pub text: String,
    pub score: f64,
    #[serde(default)]
    pub content_commit: String,
    #[serde(default)]
    pub rank: usize,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub vector_rank: Option<usize>,
    #[serde(default)]
    pub sparse_rank: Option<usize>,
}

/// Interactive `minsync stage --state-dir <DIR>` JSONL session.
pub struct StageSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageItem {
    pub key: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StageDecision {
    pub key: String,
    pub action: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub deferred: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StageCounts {
    #[serde(default)]
    pub process: usize,
    #[serde(default)]
    pub retry: usize,
    #[serde(default)]
    pub skip: usize,
    #[serde(default)]
    pub resumed: usize,
    #[serde(default)]
    pub delete: usize,
    #[serde(default)]
    pub preserve: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StageOpenResponse {
    pub counts: StageCounts,
    #[serde(default)]
    pub decisions: Vec<StageDecision>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StageFinishResponse {
    #[serde(default)]
    pub committed: usize,
    #[serde(default)]
    pub deleted: usize,
    #[serde(default)]
    pub failed: usize,
    #[serde(default)]
    pub pending: usize,
}

pub struct StageOpenParams<'a> {
    pub source_id: &'a str,
    pub fingerprint: &'a str,
    pub output_root: &'a Path,
    pub items: &'a [StageItem],
    pub unknown: &'a [String],
    pub full: bool,
    pub retry_failed: bool,
}

impl StageSession {
    pub fn spawn(binary: &str, state_dir: &Path) -> Result<Self> {
        let mut child = Command::new(binary)
            .arg("stage")
            .arg("--state-dir")
            .arg(state_dir)
            .arg("--quiet")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `{binary} stage`"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("missing minsync stage stdin"))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("missing minsync stage stdout"))?,
        );
        let mut session = Self {
            child,
            stdin,
            stdout,
        };
        let caps = session.request(&json!({ "op": "capabilities" }))?;
        ensure_stage_ok(&caps, "capabilities")?;
        Ok(session)
    }

    pub fn open(&mut self, params: &StageOpenParams<'_>) -> Result<StageOpenResponse> {
        let resp = self.request(&json!({
            "op": "open",
            "stage": "wombat-mirror",
            "source_id": params.source_id,
            "fingerprint": params.fingerprint,
            "output_root": params.output_root,
            "items": params.items,
            "unknown": params.unknown,
            "full": params.full,
            "wait": true,
            "retry_failed": params.retry_failed,
        }))?;
        ensure_stage_ok(&resp, "open")?;
        let parsed: StageOpenResponse = serde_json::from_value(resp)?;
        Ok(parsed)
    }

    pub fn commit(
        &mut self,
        key: &str,
        version: &str,
        rel_output_path: &str,
        size: u64,
        sha256_hex: &str,
    ) -> Result<()> {
        let resp = self.request(&json!({
            "op": "commit",
            "key": key,
            "version": version,
            "output": {
                "path": rel_output_path,
                "size": size,
                "digest": format!("sha256:{sha256_hex}"),
            }
        }))?;
        ensure_stage_ok(&resp, "commit")
    }

    pub fn skip(&mut self, key: &str, version: &str, reason: &str) -> Result<()> {
        let resp = self.request(&json!({
            "op": "skip",
            "key": key,
            "version": version,
            "reason": reason,
        }))?;
        ensure_stage_ok(&resp, "skip")
    }

    pub fn fail(&mut self, key: &str, version: &str, error: &str, defer: bool) -> Result<()> {
        let resp = self.request(&json!({
            "op": "fail",
            "key": key,
            "version": version,
            "error": error,
            "defer": defer,
            "discard_output": false,
        }))?;
        ensure_stage_ok(&resp, "fail")
    }

    pub fn delete(&mut self, key: &str) -> Result<bool> {
        let resp = self.request(&json!({
            "op": "delete",
            "key": key,
        }))?;
        ensure_stage_ok(&resp, "delete")?;
        Ok(resp
            .get("output_removed")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// Every key the stage durably holds (committed, skipped or failed), via the read-only
    /// `inspect` op. Used to preserve items outside an agent-scoped run.
    pub fn known_keys(&mut self, source_id: &str, output_root: &Path) -> Result<Vec<String>> {
        let resp = self.request(&json!({
            "op": "inspect",
            "stage": "wombat-mirror",
            "source_id": source_id,
            "output_root": output_root,
        }))?;
        ensure_stage_ok(&resp, "inspect")?;
        Ok(resp
            .get("known")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("key").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn finish(mut self) -> Result<StageFinishResponse> {
        let resp = self.request(&json!({ "op": "finish" }))?;
        ensure_stage_ok(&resp, "finish")?;
        drop(self.stdin);
        let _ = self.child.wait();
        let parsed: StageFinishResponse = serde_json::from_value(resp)?;
        Ok(parsed)
    }

    fn request(&mut self, payload: &Value) -> Result<Value> {
        let mut line = serde_json::to_string(payload)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .context("failed writing to minsync stage stdin")?;
        self.stdin.flush()?;

        let mut resp_line = String::new();
        let n = self
            .stdout
            .read_line(&mut resp_line)
            .context("failed reading minsync stage stdout")?;
        if n == 0 {
            bail!("minsync stage closed stdout unexpectedly");
        }
        let v: Value = serde_json::from_str(&resp_line)
            .with_context(|| format!("invalid JSON from minsync stage: {resp_line:?}"))?;
        Ok(v)
    }
}

fn ensure_stage_ok(resp: &Value, op: &str) -> Result<()> {
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let code = resp
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let msg = resp
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("unknown stage error");
    bail!("minsync stage {op} failed [{code}]: {msg}")
}

fn stderr_or_stdout(output: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !err.is_empty() {
        err
    } else {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}
