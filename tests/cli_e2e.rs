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
    for d in [&codex_dir, &claude_dir, &oc_dir, &pi_dir] {
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
    assert_eq!(sync_json["total_sessions"], 4);
    assert_eq!(sync_json["total_messages"], 8);
    assert_eq!(sync_json["written"], 4);

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
    assert!(idx_json["chunkCount"].as_u64().unwrap() >= 4);

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
    assert_eq!(inc_json["total_messages"], 9);

    // minsync must re-index only the appended file, not the other three sessions.
    let idx = &inc_json["indexed"];
    assert_eq!(idx["files_processed"], 1, "{idx}");
    assert_eq!(idx["already_up_to_date"], false);
    assert!(idx["chunks_added"].as_u64().unwrap() + idx["chunks_updated"].as_u64().unwrap() >= 1);
    let again_out = wombat_cmd()
        .args(common_args)
        .args(["index", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let again: Value = serde_json::from_slice(&again_out).unwrap();
    assert_eq!(again["already_up_to_date"], true, "{again}");
    assert_eq!(again["files_processed"], 0);
    assert_eq!(again["embedded_texts"], 0);

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

/// Live pipeline for the Hermes and Aside sources: sync -> minsync index -> search -> trace,
/// proving reasoning/tool noise stays out of the index and `--agent` scoping works.
#[test]
fn cli_end_to_end_hermes_and_aside_sources() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let hermes_home = tmp.path().join("hermes");
    let aside_home = tmp.path().join("aside");
    std::fs::create_dir_all(&hermes_home).unwrap();

    {
        let conn = Connection::open(hermes_home.join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, parent_session_id TEXT, started_at REAL NOT NULL, ended_at REAL, title TEXT, cwd TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, tool_calls TEXT, tool_name TEXT, tool_call_id TEXT, timestamp REAL NOT NULL, active INTEGER NOT NULL DEFAULT 1, display_kind TEXT);
             INSERT INTO sessions VALUES ('h1', 'discord', NULL, 1777690945.5, 1777690999.0, 'hermes chat', '/Users/a/proj');
             INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES
               ('h1', 'user', 'hermes_umbra_ledger 는 뭐하는 거야?', NULL, 1777690946.0),
               ('h1', 'assistant', '', '[{\"name\":\"HERMES_TOOLCALL_NEVER_INDEX\"}]', 1777690947.0),
               ('h1', 'tool', 'HERMES_TOOLRESULT_NEVER_INDEX', NULL, 1777690948.0),
               ('h1', 'assistant', 'hermes_umbra_ledger는 증분 원장입니다.', NULL, 1777690949.0);",
        )
        .unwrap();
    }

    let acct = aside_home.join("u/0");
    std::fs::create_dir_all(acct.join("sessions/2026-11-01_as1")).unwrap();
    {
        let conn = Connection::open(acct.join("state.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, trigger TEXT, ephemeral INTEGER NOT NULL DEFAULT 0, created_at INTEGER, updated_at INTEGER, cwd TEXT);
             INSERT INTO sessions VALUES ('as1', NULL, 'aside chat', '{\"type\":\"user\"}', 0, 1793536840, 1793536999, '/x');",
        )
        .unwrap();
    }
    let lines = [
        json!({"role": "user", "content": "aside_nimbus_tab 예약 페이지 열어줘", "timestamp": 1793536840200i64}),
        json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "ASIDE_THINKING_NEVER_INDEX"}, {"type": "toolCall", "id": "c1", "name": "repl", "arguments": {"code": "ASIDE_TOOLCALL_NEVER_INDEX"}}], "timestamp": 1793536840400i64}),
        json!({"role": "toolResult", "toolCallId": "c1", "toolName": "repl", "content": [{"type": "text", "text": "ASIDE_TOOLRESULT_NEVER_INDEX"}], "timestamp": 1793536840500i64}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "aside_nimbus_tab 페이지를 열었습니다."}], "timestamp": 1793536840600i64}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(acct.join("sessions/2026-11-01_as1/messages.jsonl"), body).unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("hermes={}", hermes_home.display()),
        "--source",
        &format!("aside={}", aside_home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };

    let sync: Value = serde_json::from_slice(&run(&["sync", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 2);
    assert_eq!(sync["total_messages"], 4);
    run(&["index", "--json"]);

    let search = |query: &str, agent: Option<&str>| -> Vec<Value> {
        let mut extra = vec!["search", "keyword", query, "--json"];
        if let Some(a) = agent {
            extra.extend(["--agent", a]);
        }
        serde_json::from_slice(&run(&extra)).unwrap()
    };

    let hits = search("aside_nimbus_tab", None);
    assert_eq!(hits[0]["agent"], "aside");
    assert_eq!(hits[0]["session_id"], "as1");
    let hits = search("hermes_umbra_ledger", None);
    assert_eq!(hits[0]["agent"], "hermes");
    assert_eq!(hits[0]["session_id"], "h1");

    // Agent scoping: every hit of an aside-scoped search belongs to aside, even for a query
    // that only matches the hermes session (sparse retrieval always returns nearest chunks).
    let scoped = search("hermes_umbra_ledger", Some("aside"));
    assert!(scoped.iter().all(|h| h["agent"] == "aside"));
    assert!(!search("aside_nimbus_tab", Some("aside")).is_empty());
    assert!(
        search("aside_nimbus_tab", Some("hermes"))
            .iter()
            .all(|h| h["agent"] == "hermes")
    );

    // Reasoning / tool payloads must never reach the indexed text.
    for noise in [
        "ASIDE_THINKING_NEVER_INDEX",
        "ASIDE_TOOLCALL_NEVER_INDEX",
        "ASIDE_TOOLRESULT_NEVER_INDEX",
        "HERMES_TOOLCALL_NEVER_INDEX",
        "HERMES_TOOLRESULT_NEVER_INDEX",
    ] {
        assert!(
            search(noise, None).iter().all(|h| {
                !h["snippet"].as_str().unwrap().contains(noise)
                    && !h["content"].as_str().unwrap().contains(noise)
            }),
            "{noise} was indexed"
        );
    }

    // Trace of the aside assistant turn exposes the raw tool call that the index hides.
    let trace: Value =
        serde_json::from_slice(&run(&["trace", "aside:as1", "--turn", "2", "--json"])).unwrap();
    let raw = trace["raw_trace"].as_str().unwrap();
    assert!(raw.contains("ASIDE_TOOLCALL_NEVER_INDEX") && raw.contains("페이지를 열었습니다"));
    let trace: Value =
        serde_json::from_slice(&run(&["trace", "hermes:h1", "--turn", "2", "--json"])).unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("HERMES_TOOLCALL_NEVER_INDEX")
    );
}

/// Live pipeline for Gemini CLI chat logs, including `$rewindTo` / `$patch` replay.
#[test]
fn cli_end_to_end_gemini_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("gemini");
    let chats = home.join("tmp/proj/chats");
    std::fs::create_dir_all(&chats).unwrap();
    let user = |id: &str, t: &str| json!({"id": id, "timestamp": "2026-11-05T01:02:04.000Z", "type": "user", "content": [{"text": t}]});
    let lines = [
        json!({"sessionId": "gem-e2e-1", "projectHash": "h", "startTime": "2026-11-05T01:02:03.000Z", "lastUpdated": "2026-11-05T01:02:03.000Z", "kind": "main", "directories": ["/Users/a/proj"]}),
        user("u1", "gemini_zephyr_probe 설명해줘"),
        json!({"id": "g1", "timestamp": "2026-11-05T01:02:05.000Z", "type": "gemini", "content": "gemini_zephyr_probe는 검증용 용어입니다.",
               "thoughts": [{"subject": "GEMINI_E2E_THOUGHT_NEVER_INDEX"}],
               "toolCalls": [{"id": "t1", "name": "shell", "args": {"command": "GEMINI_E2E_TOOLCALL_NEVER_INDEX"}}]}),
        user("u2", "gemini_rewound_text_never_indexed"),
        json!({"$rewindTo": "u2"}),
        json!({"id": "g2", "timestamp": "2026-11-05T01:02:09.000Z", "type": "gemini", "content": "초안"}),
        json!({"$patch": {"id": "g2", "content": "gemini_final_patched_answer"}}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(chats.join("session-2026-11-05T01-02-gemE2E01.jsonl"), body).unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("gemini={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 3);

    let search = |q: &str, agent: Option<&str>| -> Vec<Value> {
        let mut extra = vec!["search", "keyword", q, "--json"];
        if let Some(a) = agent {
            extra.extend(["--agent", a]);
        }
        serde_json::from_slice(&run(&extra)).unwrap()
    };
    let hits = search("gemini_zephyr_probe", Some("gemini"));
    assert_eq!(hits[0]["agent"], "gemini");
    assert_eq!(hits[0]["session_id"], "gem-e2e-1");
    assert_eq!(hits[0]["cwd"], "/Users/a/proj");
    assert!(!search("gemini_final_patched_answer", Some("gemini")).is_empty());
    for noise in [
        "GEMINI_E2E_THOUGHT_NEVER_INDEX",
        "GEMINI_E2E_TOOLCALL_NEVER_INDEX",
        "gemini_rewound_text_never_indexed",
    ] {
        assert!(
            search(noise, None)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run(&[
        "trace",
        "gemini:gem-e2e-1",
        "--turn",
        "2",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("GEMINI_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// Kilo Code CLI shares OpenCode's tables but must index as its own agent, beside OpenCode.
#[test]
fn cli_end_to_end_kilo_source_stays_separate_from_opencode() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let kilo_dir = tmp.path().join("kilo");
    let oc_dir = tmp.path().join("opencode");
    std::fs::create_dir_all(&kilo_dir).unwrap();
    std::fs::create_dir_all(&oc_dir).unwrap();
    let mk = |db: &Path, sid: &str, user: &str, answer: &str, noise: &str| {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute("INSERT INTO session VALUES (?1, 'p', NULL, 's', '/Users/a/proj', 't', '1', 1794577663297, 1794577679646)", params![sid]).unwrap();
        let ins_m = |id: &str, ts: i64, data: Value| {
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
                params![id, sid, ts, data.to_string()],
            )
            .unwrap();
        };
        let ins_p = |id: &str, mid: &str, ts: i64, data: Value| {
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![id, mid, sid, ts, data.to_string()],
            )
            .unwrap();
        };
        ins_m(
            "m1",
            1794577663321,
            json!({"role": "user", "time": {"created": 1794577663321i64}}),
        );
        ins_p(
            "p1",
            "m1",
            1794577663322,
            json!({"type": "text", "text": user}),
        );
        ins_m(
            "m2",
            1794577663400,
            json!({"role": "assistant", "parentID": "m1", "time": {"created": 1794577663400i64}}),
        );
        ins_p(
            "p2",
            "m2",
            1794577663401,
            json!({"type": "tool", "tool": "bash", "callID": "c", "state": {"status": "completed", "input": {"command": noise}, "output": noise}}),
        );
        ins_p(
            "p3",
            "m2",
            1794577663402,
            json!({"type": "text", "text": answer}),
        );
    };
    mk(
        &kilo_dir.join("kilo.db"),
        "ses_kilo_e2e",
        "kilo_zephyr_probe 설명해줘",
        "kilo_zephyr_probe는 검증용입니다.",
        "KILO_E2E_TOOL_NEVER_INDEX",
    );
    mk(
        &oc_dir.join("opencode.db"),
        "ses_oc_e2e",
        "opencode_zephyr_probe 설명해줘",
        "opencode_zephyr_probe는 검증용입니다.",
        "OC_E2E_TOOL_NEVER_INDEX",
    );

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("kilo={}", kilo_dir.display()),
        "--source",
        &format!("opencode={}", oc_dir.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 2);
    assert_eq!(sync["total_messages"], 4);
    let search = |q: &str, agent: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&["search", "keyword", q, "--agent", agent, "--json"])).unwrap()
    };
    let k = search("kilo_zephyr_probe", "kilo");
    assert_eq!(k[0]["agent"], "kilo");
    assert_eq!(k[0]["session_id"], "ses_kilo_e2e");
    assert!(
        search("kilo_zephyr_probe", "kilo")
            .iter()
            .all(|h| h["agent"] == "kilo")
    );
    let o = search("opencode_zephyr_probe", "opencode");
    assert_eq!(o[0]["agent"], "opencode");
    assert!(o.iter().all(|h| h["agent"] == "opencode"));
    for (noise, agent) in [
        ("KILO_E2E_TOOL_NEVER_INDEX", "kilo"),
        ("OC_E2E_TOOL_NEVER_INDEX", "opencode"),
    ] {
        assert!(
            search(noise, agent)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
}

/// Live pipeline for Qwen Code sessions through real minsync.
#[test]
fn cli_end_to_end_qwen_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("qwen");
    let chats = home.join("projects/-Users-a-proj/chats");
    std::fs::create_dir_all(&chats).unwrap();
    let rec = |uuid: &str, typ: &str, role: &str, parts: Value| {
        json!({"uuid": uuid, "sessionId": "qwen-e2e-1", "timestamp": "2026-11-13T00:00:00.000Z",
               "type": typ, "cwd": "/Users/a/proj", "version": "0.27.3", "provenance": "real_user",
               "message": {"role": role, "parts": parts}})
    };
    let lines = [
        rec(
            "1",
            "user",
            "user",
            json!([{"text": "qwen_zephyr_probe 설명해줘"}]),
        ),
        rec(
            "2",
            "assistant",
            "model",
            json!([{"text": "QWEN_E2E_THOUGHT_NEVER_INDEX", "thought": true}, {"text": "qwen_zephyr_probe는 검증용 용어입니다."}]),
        ),
        rec(
            "3",
            "assistant",
            "model",
            json!([{"functionCall": {"name": "shell", "args": {"command": "QWEN_E2E_TOOLCALL_NEVER_INDEX"}}}]),
        ),
        rec(
            "4",
            "system",
            "user",
            json!([{"text": "QWEN_E2E_SNAPSHOT_NEVER_INDEX"}]),
        ),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(chats.join("qwen-e2e-1.jsonl"), body).unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("qwen={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&["search", "keyword", q, "--agent", "qwen", "--json"]))
            .unwrap()
    };
    let hits = search("qwen_zephyr_probe");
    assert_eq!(hits[0]["agent"], "qwen");
    assert_eq!(hits[0]["session_id"], "qwen-e2e-1");
    for noise in [
        "QWEN_E2E_THOUGHT_NEVER_INDEX",
        "QWEN_E2E_TOOLCALL_NEVER_INDEX",
        "QWEN_E2E_SNAPSHOT_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value =
        serde_json::from_slice(&run(&["trace", "qwen:qwen-e2e-1", "--turn", "2", "--json"]))
            .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("QWEN_E2E_THOUGHT_NEVER_INDEX")
    );
}

/// Live pipeline for Crush (per-project SQLite + projects.json index) through real minsync.
#[test]
fn cli_end_to_end_crush_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let proj = tmp.path().join("work/proj");
    let crush_dir = proj.join(".crush");
    std::fs::create_dir_all(&crush_dir).unwrap();
    {
        let conn = Connection::open(crush_dir.join("crush.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, created_at INTEGER NOT NULL);
             CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL, parts TEXT NOT NULL DEFAULT '[]', model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, is_summary_message INTEGER DEFAULT 0 NOT NULL);
             INSERT INTO sessions VALUES ('crush-e2e-1', NULL, 'crush chat', 0, 1794576811, 1794576798);",
        )
        .unwrap();
        for (id, role, parts, ts) in [
            (
                "c1",
                "user",
                json!([{"type": "text", "data": {"text": "crush_zephyr_probe 설명해줘"}}]),
                1794576798,
            ),
            (
                "c2",
                "assistant",
                json!([{"type": "reasoning", "data": {"thinking": "CRUSH_E2E_THINKING_NEVER_INDEX"}}, {"type": "tool_call", "data": {"name": "bash", "input": "CRUSH_E2E_TOOLCALL_NEVER_INDEX"}}]),
                1794576799,
            ),
            (
                "c3",
                "tool",
                json!([{"type": "tool_result", "data": {"content": "CRUSH_E2E_TOOLRESULT_NEVER_INDEX"}}]),
                1794576800,
            ),
            (
                "c4",
                "assistant",
                json!([{"type": "text", "data": {"text": "crush_zephyr_probe는 검증용 용어입니다."}}]),
                1794576801,
            ),
        ] {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, parts, created_at, updated_at) VALUES (?1, 'crush-e2e-1', ?2, ?3, ?4, ?4)",
                params![id, role, parts.to_string(), ts],
            )
            .unwrap();
        }
    }
    let share = tmp.path().join("share/crush");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(
        share.join("projects.json"),
        json!({"projects": [{"path": proj, "data_dir": crush_dir}]}).to_string(),
    )
    .unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("crush={}", share.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .arg("--include-ephemeral")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let run_plain = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    // tmp dirs are "ephemeral" projects, so sync must opt in.
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run_plain(&[
            "search", "keyword", q, "--agent", "crush", "--json",
        ]))
        .unwrap()
    };
    let hits = search("crush_zephyr_probe");
    assert_eq!(hits[0]["agent"], "crush");
    assert_eq!(hits[0]["session_id"], "crush-e2e-1");
    for noise in [
        "CRUSH_E2E_THINKING_NEVER_INDEX",
        "CRUSH_E2E_TOOLCALL_NEVER_INDEX",
        "CRUSH_E2E_TOOLRESULT_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run_plain(&[
        "trace",
        "crush:crush-e2e-1",
        "--turn",
        "2",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("CRUSH_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// Live pipeline for GitHub Copilot CLI sessions through real minsync.
#[test]
fn cli_end_to_end_copilot_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("copilot");
    let sd = home.join("session-state/cop-e2e-1");
    std::fs::create_dir_all(&sd).unwrap();
    let ev = |ty: &str, data: Value| json!({"type": ty, "id": ty, "timestamp": "2026-11-13T00:00:00.000Z", "data": data});
    let lines = [
        ev(
            "session.start",
            json!({"sessionId": "cop-e2e-1", "copilotVersion": "1.0.110", "context": {"cwd": "/Users/a/proj"}}),
        ),
        ev(
            "user.message",
            json!({"content": "copilot_zephyr_probe 설명해줘", "transformedContent": "<current_datetime>x</current_datetime> copilot_zephyr_probe"}),
        ),
        ev(
            "assistant.message",
            json!({"content": "", "toolRequests": [{"toolCallId": "c1", "name": "bash", "arguments": {"command": "COPILOT_E2E_TOOLCALL_NEVER_INDEX"}}]}),
        ),
        ev(
            "tool.execution_complete",
            json!({"toolCallId": "c1", "success": true, "result": {"content": "COPILOT_E2E_TOOLRESULT_NEVER_INDEX"}}),
        ),
        ev(
            "assistant.message",
            json!({"content": "copilot_zephyr_probe는 검증용 용어입니다.", "reasoningText": "COPILOT_E2E_REASONING_NEVER_INDEX", "toolRequests": []}),
        ),
        ev(
            "system.message",
            json!({"content": "COPILOT_E2E_SYSTEM_NEVER_INDEX"}),
        ),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(sd.join("events.jsonl"), body).unwrap();
    std::fs::write(
        sd.join("workspace.yaml"),
        "id: cop-e2e-1\ncwd: /Users/a/proj\nname: copilot chat\n",
    )
    .unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("copilot={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&[
            "search", "keyword", q, "--agent", "copilot", "--json",
        ]))
        .unwrap()
    };
    let hits = search("copilot_zephyr_probe");
    assert_eq!(hits[0]["agent"], "copilot");
    assert_eq!(hits[0]["session_id"], "cop-e2e-1");
    assert_eq!(hits[0]["title"], "copilot chat");
    for noise in [
        "COPILOT_E2E_TOOLCALL_NEVER_INDEX",
        "COPILOT_E2E_TOOLRESULT_NEVER_INDEX",
        "COPILOT_E2E_REASONING_NEVER_INDEX",
        "COPILOT_E2E_SYSTEM_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run(&[
        "trace",
        "copilot:cop-e2e-1",
        "--turn",
        "2",
        "--context",
        "3",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("COPILOT_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// Live pipeline for Continue CLI sessions (one JSON file per session) through real minsync.
#[test]
fn cli_end_to_end_continue_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("continue");
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let h = |role: &str, content: Value, extra: Value| {
        let mut msg = json!({"role": role, "content": content});
        if let (Some(m), Some(e)) = (msg.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        json!({"message": msg, "contextItems": []})
    };
    std::fs::write(
        sessions.join("cn-e2e-1.json"),
        json!({"sessionId": "cn-e2e-1", "title": "continue chat", "workspaceDirectory": "/Users/a/proj", "history": [
            h("user", json!("continue_zephyr_probe 설명해줘"), json!({})),
            h("assistant", json!(""), json!({"toolCalls": [{"id": "c1", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"CONTINUE_E2E_TOOLCALL_NEVER_INDEX\"}"}}]})),
            h("tool", json!("CONTINUE_E2E_TOOLRESULT_NEVER_INDEX"), json!({})),
            h("assistant", json!("continue_zephyr_probe는 검증용 용어입니다."), json!({})),
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        sessions.join("sessions.json"),
        json!([{"sessionId": "cn-e2e-1", "title": "continue chat", "dateCreated": "1794577976115", "workspaceDirectory": "/Users/a/proj", "messageCount": 2}]).to_string(),
    )
    .unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("continue={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(
        sync["total_sessions"], 1,
        "sessions.json index must not count as a session"
    );
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&[
            "search", "keyword", q, "--agent", "continue", "--json",
        ]))
        .unwrap()
    };
    let hits = search("continue_zephyr_probe");
    assert_eq!(hits[0]["agent"], "continue");
    assert_eq!(hits[0]["session_id"], "cn-e2e-1");
    assert_eq!(hits[0]["title"], "continue chat");
    for noise in [
        "CONTINUE_E2E_TOOLCALL_NEVER_INDEX",
        "CONTINUE_E2E_TOOLRESULT_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run(&[
        "trace",
        "continue:cn-e2e-1",
        "--turn",
        "2",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("CONTINUE_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// iFlow CLI writes Claude-Code-shaped logs but must index as its own agent beside Claude Code.
#[test]
fn cli_end_to_end_iflow_source_stays_separate_from_claude_code() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let iflow_home = tmp.path().join("iflow");
    let cc_root = tmp.path().join("claude/proj-a");
    std::fs::create_dir_all(iflow_home.join("projects/-Users-a-proj")).unwrap();
    std::fs::create_dir_all(&cc_root).unwrap();
    let mk = |path: &Path, sid: &str, user: &str, answer: &str, noise: &str| {
        let rec = |uuid: &str, ty: &str, content: Value| {
            json!({"uuid": uuid, "sessionId": sid, "timestamp": "2026-11-13T00:00:00.000Z", "type": ty,
                   "isSidechain": false, "cwd": "/Users/a/proj", "message": {"role": ty, "content": content}})
        };
        let lines = [
            rec("1", "user", json!(user)),
            rec(
                "2",
                "assistant",
                json!([{"type": "tool_use", "id": "t", "name": "bash", "input": {"command": noise}}, {"type": "text", "text": answer}]),
            ),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, body).unwrap();
    };
    mk(
        &iflow_home.join("projects/-Users-a-proj/session-if-e2e.jsonl"),
        "session-if-e2e",
        "iflow_zephyr_probe 설명해줘",
        "iflow_zephyr_probe는 검증용입니다.",
        "IFLOW_E2E_TOOL_NEVER_INDEX",
    );
    mk(
        &cc_root.join("cc-e2e.jsonl"),
        "cc-e2e",
        "claude_zephyr_probe 설명해줘",
        "claude_zephyr_probe는 검증용입니다.",
        "CC_E2E_TOOL_NEVER_INDEX",
    );

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("iflow={}", iflow_home.display()),
        "--source",
        &format!("claude-code={}", tmp.path().join("claude").display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 2);
    assert_eq!(sync["total_messages"], 4);
    let search = |q: &str, agent: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&["search", "keyword", q, "--agent", agent, "--json"])).unwrap()
    };
    let i = search("iflow_zephyr_probe", "iflow");
    assert_eq!(i[0]["agent"], "iflow");
    assert_eq!(i[0]["session_id"], "session-if-e2e");
    assert!(i.iter().all(|h| h["agent"] == "iflow"));
    let c = search("claude_zephyr_probe", "claude-code");
    assert_eq!(c[0]["agent"], "claude-code");
    assert!(c.iter().all(|h| h["agent"] == "claude-code"));
    for (noise, agent) in [
        ("IFLOW_E2E_TOOL_NEVER_INDEX", "iflow"),
        ("CC_E2E_TOOL_NEVER_INDEX", "claude-code"),
    ] {
        assert!(
            search(noise, agent)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
}

/// Live pipeline for CodeBuddy Code sessions through real minsync.
#[test]
fn cli_end_to_end_codebuddy_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("codebuddy");
    let proj = home.join("projects/-Users-a-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let msg = |id: &str, role: &str, content: Value| json!({"id": id, "type": "message", "role": role, "timestamp": "1794578348516", "sessionId": "cb-e2e-1", "cwd": "/Users/a/proj", "content": content});
    let lines = [
        msg(
            "1",
            "user",
            json!([{"type": "input_text", "text": "codebuddy_zephyr_probe 설명해줘"}]),
        ),
        json!({"type": "function_call", "id": "f", "callId": "c", "name": "Bash", "arguments": "{\"command\":\"CODEBUDDY_E2E_TOOLCALL_NEVER_INDEX\"}"}),
        json!({"type": "function_call_result", "callId": "c", "name": "Bash", "status": "completed", "output": "CODEBUDDY_E2E_TOOLRESULT_NEVER_INDEX"}),
        msg(
            "2",
            "assistant",
            json!([{"type": "output_text", "text": "codebuddy_zephyr_probe는 검증용 용어입니다."}]),
        ),
        json!({"type": "summary", "id": "s", "summary": "CODEBUDDY_E2E_SUMMARY_NEVER_INDEX"}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(proj.join("cb-e2e-1.jsonl"), body).unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("codebuddy={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&[
            "search",
            "keyword",
            q,
            "--agent",
            "codebuddy",
            "--json",
        ]))
        .unwrap()
    };
    let hits = search("codebuddy_zephyr_probe");
    assert_eq!(hits[0]["agent"], "codebuddy");
    assert_eq!(hits[0]["session_id"], "cb-e2e-1");
    for noise in [
        "CODEBUDDY_E2E_TOOLCALL_NEVER_INDEX",
        "CODEBUDDY_E2E_TOOLRESULT_NEVER_INDEX",
        "CODEBUDDY_E2E_SUMMARY_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run(&[
        "trace",
        "codebuddy:cb-e2e-1",
        "--turn",
        "2",
        "--context",
        "3",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("CODEBUDDY_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// Live pipeline for Aider chat history (markdown, many sessions per file) through real minsync.
#[test]
fn cli_end_to_end_aider_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let proj = tmp.path().join("work/proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::write(
        proj.join(".aider.chat.history.md"),
        "\n# aider chat started at 2026-11-01 10:00:00\n\n> Aider v0.86.2  \n> Repo-map: AIDER_E2E_BANNER_NEVER_INDEX  \n\n#### aider_zephyr_probe 설명해줘  \n\naider_zephyr_probe는 검증용 용어입니다.\n\n> Tokens: AIDER_E2E_TOKENS_NEVER_INDEX  \n\n#### /run echo hi  \n> Add command output to the chat? (Y)es/(N)o [Yes]: n  \n\n# aider chat started at 2026-11-02 11:00:00\n\n#### aider_second_session_probe  \n\nsecond answer text\n",
    )
    .unwrap();
    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("aider={}", tmp.path().join("work").display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .arg("--include-ephemeral")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let plain = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    // temp dirs are "ephemeral" projects, so sync must opt in.
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(
        sync["total_sessions"], 2,
        "one session per '# aider chat started at' header"
    );
    assert_eq!(sync["total_messages"], 4);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&plain(&[
            "search", "keyword", q, "--agent", "aider", "--json",
        ]))
        .unwrap()
    };
    let hits = search("aider_zephyr_probe");
    assert_eq!(hits[0]["agent"], "aider");
    assert!(!search("aider_second_session_probe").is_empty());
    for noise in [
        "AIDER_E2E_BANNER_NEVER_INDEX",
        "AIDER_E2E_TOKENS_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
}

/// Live pipeline for Mistral Vibe's unified session store through real minsync.
#[test]
fn cli_end_to_end_vibe_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let home = tmp.path().join("vibe");
    let sid = "vibe-e2e-1";
    let store = home.join("logs/session/unified").join(sid);
    let generation = "0000000000000003";
    std::fs::create_dir_all(store.join("chunks")).unwrap();
    std::fs::create_dir_all(store.join("generations").join(generation)).unwrap();
    let chunk = "b".repeat(64);
    let h = |role: &str, content: Value| json!({"message": {"role": role, "content": content}, "source": "history", "turn_index": 0});
    let history = [
        h(
            "user",
            json!([{"type": "text", "text": "vibe_zephyr_probe 설명해줘"}]),
        ),
        h(
            "assistant",
            json!([{"type": "tool_call", "id": "t", "name": "bash", "arguments": "VIBE_E2E_TOOLCALL_NEVER_INDEX"}]),
        ),
        h(
            "tool",
            json!([{"type": "text", "text": "VIBE_E2E_TOOLRESULT_NEVER_INDEX"}]),
        ),
        h(
            "assistant",
            json!([{"type": "text", "text": "vibe_zephyr_probe는 검증용 용어입니다."}]),
        ),
    ];
    std::fs::write(
        store.join("chunks").join(format!("{chunk}.json")),
        serde_json::to_string(&history).unwrap(),
    )
    .unwrap();
    std::fs::write(
        store
            .join("generations")
            .join(generation)
            .join("checkpoint.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(
        store.join("generations").join(generation).join("manifest.json"),
        json!({"generation": generation, "session_id": sid, "checkpoint": {"path": "checkpoint.json", "chunks": [chunk]}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        store.join("CURRENT"),
        json!({"generation": generation, "snapshot_sequence": 3, "session_id": sid}).to_string(),
    )
    .unwrap();
    std::fs::write(
        store.join("meta.json"),
        json!({"session_id": sid, "parent_session_id": null, "start_time": "2026-11-13T14:15:51.772000+00:00", "environment": {"working_directory": "/Users/a/proj"}, "title": "vibe chat"}).to_string(),
    )
    .unwrap();

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("vibe={}", home.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(sync["total_sessions"], 1);
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&["search", "keyword", q, "--agent", "vibe", "--json"]))
            .unwrap()
    };
    let hits = search("vibe_zephyr_probe");
    assert_eq!(hits[0]["agent"], "vibe");
    assert_eq!(hits[0]["session_id"], sid);
    assert_eq!(hits[0]["title"], "vibe chat");
    for noise in [
        "VIBE_E2E_TOOLCALL_NEVER_INDEX",
        "VIBE_E2E_TOOLRESULT_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value =
        serde_json::from_slice(&run(&["trace", "vibe:vibe-e2e-1", "--turn", "2", "--json"]))
            .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("VIBE_E2E_TOOLCALL_NEVER_INDEX")
    );
}

/// Live pipeline for Goose's `sessions.db` (SQLite) through real minsync.
#[test]
fn cli_end_to_end_goose_source() {
    if !minsync_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("wombat-data");
    let goose = tmp.path().join("goose");
    let sessions = goose.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    {
        let conn = Connection::open(sessions.join("sessions.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', session_type TEXT NOT NULL DEFAULT 'user', working_dir TEXT NOT NULL, created_at TIMESTAMP, updated_at TIMESTAMP, parent_session_id TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, message_id TEXT, session_id TEXT NOT NULL, role TEXT NOT NULL, content_json TEXT NOT NULL, created_timestamp INTEGER NOT NULL, metadata_json TEXT);
             INSERT INTO sessions VALUES ('goose-e2e-1', 'goose chat', 'user', '/Users/a/proj', '2026-11-13 14:00:00', '2026-11-13 14:05:00', NULL);
             INSERT INTO sessions VALUES ('goose-hidden', 'title gen', 'hidden', '/Users/a/proj', '2026-11-13 14:00:00', '2026-11-13 14:05:00', NULL);",
        )
        .unwrap();
        let add = |sid: &str, role: &str, blocks: Value, ts: i64, meta: Value| {
            conn.execute(
                "INSERT INTO messages (session_id, role, content_json, created_timestamp, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sid, role, blocks.to_string(), ts, meta.to_string()],
            )
            .unwrap();
        };
        let vis = json!({"userVisible": true, "agentVisible": true});
        add(
            "goose-e2e-1",
            "user",
            json!([{"type": "text", "text": "goose_zephyr_probe 설명해줘"}]),
            1794579714,
            vis.clone(),
        );
        add(
            "goose-e2e-1",
            "user",
            json!([{"type": "text", "text": "<turn-context> GOOSE_E2E_CONTEXT_NEVER_INDEX </turn-context>"}]),
            1794579715,
            json!({"userVisible": false, "agentVisible": true, "turnContext": true}),
        );
        add(
            "goose-e2e-1",
            "assistant",
            json!([{"type": "toolRequest", "id": "t", "toolCall": {"value": {"name": "shell", "arguments": {"command": "GOOSE_E2E_TOOLCALL_NEVER_INDEX"}}}}]),
            1794579716,
            vis.clone(),
        );
        add(
            "goose-e2e-1",
            "user",
            json!([{"type": "toolResponse", "id": "t", "toolResult": {"value": {"content": [{"type": "text", "text": "GOOSE_E2E_TOOLRESULT_NEVER_INDEX"}]}}}]),
            1794579717,
            vis.clone(),
        );
        add(
            "goose-e2e-1",
            "assistant",
            json!([{"type": "text", "text": "goose_zephyr_probe는 검증용 용어입니다."}]),
            1794579718,
            vis.clone(),
        );
        add(
            "goose-hidden",
            "user",
            json!([{"type": "text", "text": "GOOSE_E2E_HIDDEN_NEVER_INDEX"}]),
            1794579719,
            vis,
        );
    }

    let args = [
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--embedder",
        "none",
        "--source",
        &format!("goose={}", goose.display()),
    ];
    let run = |extra: &[&str]| -> Vec<u8> {
        wombat_cmd()
            .args(args)
            .args(extra)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    };
    let sync: Value = serde_json::from_slice(&run(&["sync", "--index", "--json"])).unwrap();
    assert_eq!(
        sync["total_sessions"], 1,
        "hidden sessions must not be indexed"
    );
    assert_eq!(sync["total_messages"], 2);
    let search = |q: &str| -> Vec<Value> {
        serde_json::from_slice(&run(&[
            "search", "keyword", q, "--agent", "goose", "--json",
        ]))
        .unwrap()
    };
    let hits = search("goose_zephyr_probe");
    assert_eq!(hits[0]["agent"], "goose");
    assert_eq!(hits[0]["session_id"], "goose-e2e-1");
    assert_eq!(hits[0]["title"], "goose chat");
    for noise in [
        "GOOSE_E2E_CONTEXT_NEVER_INDEX",
        "GOOSE_E2E_TOOLCALL_NEVER_INDEX",
        "GOOSE_E2E_TOOLRESULT_NEVER_INDEX",
        "GOOSE_E2E_HIDDEN_NEVER_INDEX",
    ] {
        assert!(
            search(noise)
                .iter()
                .all(|h| !h["content"].as_str().unwrap().contains(noise)),
            "{noise} was indexed"
        );
    }
    let trace: Value = serde_json::from_slice(&run(&[
        "trace",
        "goose:goose-e2e-1",
        "--turn",
        "2",
        "--json",
    ]))
    .unwrap();
    assert!(
        trace["raw_trace"]
            .as_str()
            .unwrap()
            .contains("GOOSE_E2E_TOOLCALL_NEVER_INDEX")
    );
}
