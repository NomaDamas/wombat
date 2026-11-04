use assert_cmd::Command;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

fn wombat_cmd() -> Command {
    assert_cmd::cargo::cargo_bin_cmd!("wombat")
}

fn minsync_available() -> bool {
    std::process::Command::new("minsync")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_zst_lines(path: &Path, lines: &[Value]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = zstd::stream::write::Encoder::new(file, 3).unwrap();
    for line in lines {
        let s = serde_json::to_string(line).unwrap();
        enc.write_all(s.as_bytes()).unwrap();
        enc.write_all(b"\n").unwrap();
    }
    enc.finish().unwrap();
}

#[test]
fn cli_end_to_end_autorag_datasource_contract_and_incremental_append() {
    if !minsync_available() {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let sources_dir = tmp.path().join("sources");
    let codex_dir = sources_dir.join("codex");
    let claude_dir = sources_dir.join("claude/proj-a");
    let oc_dir = sources_dir.join("opencode");
    let pi_dir = sources_dir.join("pi/--Volumes-SSD1-1-wombat--");
    let cursor_dir = sources_dir.join("cursor/globalStorage");
    for d in [&codex_dir, &claude_dir, &oc_dir, &pi_dir, &cursor_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // 1. Codex `.jsonl.zst`
    let codex_file = codex_dir.join("rollout-2026-10-15T12-00-00-codex-sess-01.jsonl.zst");
    let mut codex_lines = vec![
        json!({
            "timestamp": "2026-10-15T12:00:00Z",
            "type": "session_meta",
            "payload": {"session_id": "codex-sess-01", "cwd": "/Volumes/SSD1/1/wombat", "thread_source": "user"}
        }),
        json!({
            "timestamp": "2026-10-15T12:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "CODEX_SECRET_THINKING_NEVER_INDEX"}]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:00:05Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "zephyr_quasar_protocol 핸드셰이크 버그를 고쳐줘"}]
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:00:10Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "output": "TOOL_DIFF_PATCH_NOISE_99182"
            }
        }),
        json!({
            "timestamp": "2026-10-15T12:00:15Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "zephyr_quasar_protocol 타임아웃 재시도 로직을 수정했습니다."}]
            }
        }),
    ];
    write_zst_lines(&codex_file, &codex_lines);

    // 2. Claude Code `.jsonl`
    let claude_file = claude_dir.join("claude-sess-02.jsonl");
    std::fs::write(
        &claude_file,
        format!(
            "{}\n{}\n",
            json!({
                "type": "user",
                "sessionId": "claude-sess-02",
                "cwd": "/Volumes/SSD1/1/AutoRAG",
                "timestamp": "2026-10-16T01:00:00Z",
                "message": {"role": "user", "content": "baryon_lattice_cache 무효화 조건 설명해줘"}
            }),
            json!({
                "type": "assistant",
                "sessionId": "claude-sess-02",
                "timestamp": "2026-10-16T01:00:10Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "CLAUDE_HIDDEN_CHAIN_OF_THOUGHT"},
                        {"type": "text", "text": "baryon_lattice_cache는 파일 해시가 바뀔 때만 갱신됩니다."}
                    ]
                }
            })
        ),
    )
    .unwrap();

    // 3. OpenCode `opencode.db`
    {
        let conn = Connection::open(oc_dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, seq INTEGER, type TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_v2 VALUES ('oc-sess-03', NULL, '/Volumes/SSD1/1/katok', 'OpenCode Search Session', 1761000000000, 1761000010000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('ocm_1', 'oc-sess-03', 1, 'user', 1761000001000, ?1)",
            params![json!({"text": "soliton_merkle_cursor 동기화 방식 알려줘"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('ocm_2', 'oc-sess-03', 2, 'assistant', 1761000002000, ?1)",
            params![json!({"content": [{"type": "text", "text": "soliton_merkle_cursor는 스냅샷 커서로 증분 동기화합니다."}]}).to_string()],
        )
        .unwrap();
    }

    // 4. Pi `.jsonl`
    std::fs::write(
        pi_dir.join("2026-10-17T00-00-00-000Z_pi-sess-04.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            json!({"type": "session", "version": 3, "id": "pi-sess-04", "timestamp": "2026-10-17T00:00:00Z", "cwd": "/Volumes/SSD1/1/wombat"}),
            json!({"type": "message", "timestamp": "2026-10-17T00:00:01Z", "message": {"role": "user", "content": [{"type": "text", "text": "tachyon_ stage_journal 복구 동작이 뭐야?"}]}}),
            json!({"type": "message", "timestamp": "2026-10-17T00:00:05Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "tachyon_stage_journal은 중단된 지점부터 이어서 실행됩니다."}]}})
        ),
    )
    .unwrap();

    // 5. Cursor IDE `state.vscdb`
    {
        let conn = Connection::open(cursor_dir.join("state.vscdb")).unwrap();
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('composerData:cur-sess-05', ?1)",
            params![json!({
                "composerId": "cur-sess-05",
                "name": "Cursor Composer Thread",
                "createdAt": 1762000000000i64,
                "lastUpdatedAt": 1762000005000i64,
                "fullConversationHeadersOnly": [{"bubbleId": "bb1", "type": 1}, {"bubbleId": "bb2", "type": 2}]
            }).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:cur-sess-05:bb1', ?1)",
            params![json!({"type": 1, "text": "neutrino_wal_snapshot 읽기 전용 모드 확인", "workspaceProjectDir": "/Volumes/SSD1/1/MinSync"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES ('bubbleId:cur-sess-05:bb2', ?1)",
            params![json!({"type": 2, "text": "neutrino_wal_snapshot은 원본 폴더에 파일을 만들지 않습니다."}).to_string()],
        )
        .unwrap();
    }

    let common_args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("codex={}", codex_dir.display()),
        "--source",
        &format!("claude-code={}", sources_dir.join("claude").display()),
        "--source",
        &format!("opencode={}", oc_dir.display()),
        "--source",
        &format!("pi={}", sources_dir.join("pi").display()),
        "--source",
        &format!("cursor={}", sources_dir.join("cursor").display()),
    ];

    // Step 1: `wombat doctor --json` (global flags BEFORE subcommand, per AutoRAG lazykatok client)
    let doc_out = wombat_cmd()
        .args(common_args)
        .args(["doctor", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc_json: Value = serde_json::from_slice(&doc_out).unwrap();
    assert_eq!(doc_json["ready"], true);
    assert!(doc_json["freshness"].is_object());

    // Step 2: `wombat sync --json`
    let sync_out = wombat_cmd()
        .args(common_args)
        .args(["sync", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sync_json: Value = serde_json::from_slice(&sync_out).unwrap();
    assert_eq!(sync_json["total_sessions"], 5);
    assert_eq!(sync_json["total_messages"], 10);
    assert_eq!(sync_json["written"], 5);

    // Step 3: `wombat index --json`
    let idx_out = wombat_cmd()
        .args(common_args)
        .args(["index", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let idx_json: Value = serde_json::from_slice(&idx_out).unwrap();
    assert!(idx_json["chunkCount"].as_u64().unwrap() >= 5);

    // Step 4: `wombat search keyword "<query>" --json --limit 5` (bare JSON array!)
    let search_out = wombat_cmd()
        .args(common_args)
        .args([
            "search",
            "keyword",
            "zephyr_quasar_protocol",
            "--json",
            "--limit",
            "5",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits: Vec<Value> = serde_json::from_slice(&search_out).unwrap();
    assert!(!hits.is_empty());
    let top = &hits[0];
    let chunk_id = top["chunk_id"].as_str().unwrap();
    assert_eq!(top["session_id"], "codex-sess-01");
    assert_eq!(top["agent"], "codex");
    assert!(
        top["snippet"]
            .as_str()
            .unwrap()
            .contains("zephyr_quasar_protocol")
    );
    assert!(top["score"].is_number());

    // Confirm thinking/tool noise is NOT searchable!
    let noise_out = wombat_cmd()
        .args(common_args)
        .args([
            "search",
            "keyword",
            "CODEX_SECRET_THINKING_NEVER_INDEX",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let noise_hits: Vec<Value> = serde_json::from_slice(&noise_out).unwrap();
    assert!(
        noise_hits.iter().all(|h| !h["snippet"]
            .as_str()
            .unwrap()
            .contains("CODEX_SECRET_THINKING")),
        "thinking trace must never appear in indexed chunks"
    );

    // Step 5: `wombat chunk get / context / parent <chunk_id> --json`
    let cget_out = wombat_cmd()
        .args(common_args)
        .args(["chunk", "get", chunk_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cget: Value = serde_json::from_slice(&cget_out).unwrap();
    assert_eq!(cget["chunk_id"], chunk_id);
    assert_eq!(cget["session_id"], "codex-sess-01");

    let cctx_out = wombat_cmd()
        .args(common_args)
        .args(["chunk", "context", chunk_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cctx: Value = serde_json::from_slice(&cctx_out).unwrap();
    assert!(cctx["chunk"].is_object());
    assert!(cctx["previous"].is_array());
    assert!(cctx["next"].is_array());

    let cpar_out = wombat_cmd()
        .args(common_args)
        .args(["chunk", "parent", chunk_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cpar: Vec<Value> = serde_json::from_slice(&cpar_out).unwrap();
    assert_eq!(cpar.len(), 1);
    assert_eq!(cpar[0]["session_id"], "codex-sess-01");

    // Step 6: `wombat trace <session_id> --turn 2 --json` recovers full raw trace including tool output!
    let trace_out = wombat_cmd()
        .args(common_args)
        .args([
            "trace",
            "codex:codex-sess-01",
            "--turn",
            "2",
            "--context",
            "2",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace_json: Value = serde_json::from_slice(&trace_out).unwrap();
    assert!(
        trace_json["raw_trace"]
            .as_str()
            .unwrap()
            .contains("TOOL_DIFF_PATCH_NOISE_99182"),
        "full trace navigation must recover stripped tool outputs from original .jsonl.zst"
    );

    // Step 7: Incremental no-op sync processes 0 items!
    let noop_out = wombat_cmd()
        .args(common_args)
        .args(["sync", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let noop_json: Value = serde_json::from_slice(&noop_out).unwrap();
    assert_eq!(noop_json["processed"], 0);
    assert_eq!(noop_json["written"], 0);

    // Step 8: Append a new turn to Codex `.jsonl.zst` -> only 1 item re-processed, prefix-stable!
    codex_lines.push(json!({
        "timestamp": "2026-10-15T12:05:00Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hyperion_append_delta_marker 후속 검증 완료"}]
        }
    }));
    write_zst_lines(&codex_file, &codex_lines);

    let inc_out = wombat_cmd()
        .args(common_args)
        .args(["sync", "--index", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inc_json: Value = serde_json::from_slice(&inc_out).unwrap();
    assert_eq!(inc_json["processed"], 1);
    assert_eq!(inc_json["written"], 1);
    assert_eq!(inc_json["total_messages"], 11);

    let delta_hits_raw = wombat_cmd()
        .args(common_args)
        .args([
            "search",
            "keyword",
            "hyperion_append_delta_marker",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let delta_hits: Vec<Value> = serde_json::from_slice(&delta_hits_raw).unwrap();
    assert!(!delta_hits.is_empty());
    assert_eq!(delta_hits[0]["end_turn"], 3);
}
