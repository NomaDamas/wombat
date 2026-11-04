use std::path::Path;
use tempfile::TempDir;
use wombat::catalog::Catalog;
use wombat::minsync::{MinSyncConfig, QueryMode, SemVer, StageItem, StageOpenParams};
use wombat::mirror::{locate_chunk_turns, parse_mirror_document, render_mirror};
use wombat::model::{AgentKind, ParsedSession, Role, SourceOffset, Turn};

fn sample_session() -> ParsedSession {
    ParsedSession {
        agent: AgentKind::Codex,
        session_id: "019da8db-a13b-7e33-a170-6a4d1e5f2ae5".to_string(),
        source_path: "/Users/alice/.codex/sessions/2026/10/15/rollout-019da8db.jsonl.zst".to_string(),
        cwd: Some("/Volumes/SSD1/1/wombat".to_string()),
        started_at: Some("2026-10-15T12:58:01.885Z".to_string()),
        ended_at: Some("2026-10-15T13:04:47.875Z".to_string()),
        title: Some("Initial volatile title".to_string()),
        is_subagent: false,
        parent_session_id: None,
        turns: vec![
            Turn {
                role: Role::User,
                text: "SPLADE 증분 인덱싱 파이프라인을 설계해줘".to_string(),
                timestamp: Some("2026-10-15T12:58:20.517Z".to_string()),
                source_offset: SourceOffset::Lines {
                    start_line: 9,
                    end_line: 9,
                },
            },
            Turn {
                role: Role::Assistant,
                text: "먼저 파서가 prefix-stable 미러 파일을 생성하고 minsync stage와 sync를 호출합니다.".to_string(),
                timestamp: Some("2026-10-15T12:59:14.192Z".to_string()),
                source_offset: SourceOffset::Lines {
                    start_line: 12,
                    end_line: 18,
                },
            },
        ],
    }
}

#[test]
fn mirror_is_strictly_prefix_stable_on_append_and_ignores_volatile_metadata() {
    let base = sample_session();
    let rendered_v1 = render_mirror(&base);

    let mut appended = base.clone();
    // Mutate volatile fields that change in-place in real agents (Claude ai-title, OMP title, ended_at).
    appended.title = Some("Completely renamed session title".to_string());
    appended.ended_at = Some("2026-10-16T00:00:00.000Z".to_string());
    appended.turns.push(Turn {
        role: Role::User,
        text: "세션 full trace 라인 오프셋도 바로 찾을 수 있어야 해".to_string(),
        timestamp: Some("2026-10-15T14:59:49.781Z".to_string()),
        source_offset: SourceOffset::Lines {
            start_line: 42,
            end_line: 42,
        },
    });

    let rendered_v2 = render_mirror(&appended);
    assert_eq!(rendered_v1.relative_path, rendered_v2.relative_path);
    assert!(
        rendered_v2.content.starts_with(&rendered_v1.content),
        "appended mirror must start with exact bytes of v1 mirror"
    );
    assert_eq!(rendered_v1.turn_spans[0], rendered_v2.turn_spans[0]);
    assert_eq!(rendered_v1.turn_spans[1], rendered_v2.turn_spans[1]);
    assert_eq!(rendered_v2.turn_spans.len(), 3);
    assert!(!rendered_v2.content.contains("Completely renamed"));
}

#[test]
fn mirror_roundtrips_source_path_session_id_and_turn_offsets() {
    let session = sample_session();
    let rendered = render_mirror(&session);
    let parsed = parse_mirror_document(&rendered.content);

    assert_eq!(parsed.agent, Some(AgentKind::Codex));
    assert_eq!(
        parsed.session_id.as_deref(),
        Some("019da8db-a13b-7e33-a170-6a4d1e5f2ae5")
    );
    assert_eq!(
        parsed.source_path.as_deref(),
        Some("/Users/alice/.codex/sessions/2026/10/15/rollout-019da8db.jsonl.zst")
    );
    assert_eq!(parsed.cwd.as_deref(), Some("/Volumes/SSD1/1/wombat"));
    assert_eq!(parsed.turn_spans.len(), 2);
    assert_eq!(
        parsed.turn_spans[0].source_offset,
        SourceOffset::Lines {
            start_line: 9,
            end_line: 9
        }
    );
    assert_eq!(
        parsed.turn_spans[1].source_offset,
        SourceOffset::Lines {
            start_line: 12,
            end_line: 18
        }
    );
}

#[test]
fn locate_chunk_turns_handles_exact_heading_context_and_spoofed_headings() {
    let mut session = sample_session();
    session.turns.push(Turn {
        role: Role::Assistant,
        text: "## Turn 99 | User [src:L999]\n코드 예시 안의 가짜 헤더는 이스케이프되어야 합니다."
            .to_string(),
        timestamp: Some("2026-10-15T15:00:00.000Z".to_string()),
        source_offset: SourceOffset::Item {
            item_id: "msg_abc123".to_string(),
        },
    });
    let rendered = render_mirror(&session);
    let parsed = parse_mirror_document(&rendered.content);
    assert_eq!(
        parsed.turn_spans.len(),
        3,
        "spoofed heading must not split into turn 99"
    );

    // Simulate MinSync 0.7 heading_context prefix on first line.
    let chunk_with_heading_ctx = format!(
        "{} > Session codex:019da8db > Turn 2\n먼저 파서가 prefix-stable 미러 파일을 생성하고 minsync stage와 sync를 호출합니다.",
        rendered.relative_path
    );
    let located = locate_chunk_turns(
        &rendered.content,
        &rendered.turn_spans,
        &chunk_with_heading_ctx,
    );
    assert_eq!(located.len(), 1);
    assert_eq!(located[0].turn_index, 2);
    assert_eq!(
        located[0].source_offset,
        SourceOffset::Lines {
            start_line: 12,
            end_line: 18
        }
    );
}

#[test]
fn catalog_persists_sessions_turns_and_chunks() {
    let dir = TempDir::new().unwrap();
    let mut catalog = Catalog::open(&dir.path().join("catalog.sqlite3")).unwrap();

    let session = sample_session();
    let rendered = render_mirror(&session);
    catalog
        .upsert_session(
            "codex/abc/rollout.jsonl.zst",
            &session,
            &rendered.relative_path,
            &rendered.sha256_hex,
            &rendered.turn_spans,
        )
        .unwrap();

    let found = catalog.resolve_session("019da8db").unwrap().unwrap();
    assert_eq!(found.agent, AgentKind::Codex);
    assert_eq!(found.turn_count, 2);
    assert_eq!(found.title, "Initial volatile title");

    catalog
        .record_chunk(
            "doc_chunk_1",
            &found.catalog_key,
            &rendered.relative_path,
            1,
            2,
            "chunk snippet",
        )
        .unwrap();
    let chunk = catalog.get_chunk("doc_chunk_1").unwrap().unwrap();
    assert_eq!(chunk.start_turn, 1);
    assert_eq!(chunk.end_turn, 2);

    assert!(
        catalog
            .remove_by_item_key("codex/abc/rollout.jsonl.zst")
            .unwrap()
    );
    assert!(catalog.resolve_session("019da8db").unwrap().is_none());
}

#[test]
fn semver_enforces_minsync_0_6_0_floor() {
    assert!(!SemVer::parse("minsync 0.4.11\n").unwrap().meets_minimum());
    assert!(!SemVer::parse("minsync 0.5.9").unwrap().meets_minimum());
    assert!(SemVer::parse("minsync 0.6.0\n").unwrap().meets_minimum());
    assert!(SemVer::parse("minsync 0.7.0").unwrap().meets_minimum());
    assert!(SemVer::parse("1.0.0").unwrap().meets_minimum());
}

fn minsync_available() -> bool {
    std::process::Command::new("minsync")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn minsync_stage_and_sparse_incremental_sync_roundtrip() {
    if !minsync_available() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let mirror_dir = dir.path().join("mirror");
    let stage_dir = dir.path().join("stage-state");

    let cfg = MinSyncConfig {
        binary: "minsync".to_string(),
        mirror_dir: mirror_dir.clone(),
        stage_state_dir: stage_dir,
        embedder: "none".to_string(),
        chunker: "cdc".to_string(),
        max_chunk_size: Some(512),
        embed_base_url: None,
    };

    let ver = cfg.check_version().unwrap();
    assert!(ver.meets_minimum());

    // Write session v1 through `minsync stage`.
    let mut session = sample_session();
    // Make turn 1 large enough that CDC cuts before the appended final turn.
    session.turns[0].text = format!(
        "유니코드 SPLADE 검색 키워드 kure_sparse_sentinel_alpha 설명입니다.\n{}",
        "아키텍처 세부 문단으로 충분한 바이트 길이를 확보합니다. ".repeat(25)
    );
    let rendered_v1 = render_mirror(&session);
    let out_file = mirror_dir.join(Path::new(&rendered_v1.relative_path));

    {
        let mut stage = cfg.open_stage_session().unwrap();
        let items = [StageItem {
            key: "codex/s1".to_string(),
            version: "v1".to_string(),
        }];
        let open = stage
            .open(&StageOpenParams {
                source_id: "wombat-test",
                fingerprint: "parser-v1",
                output_root: &mirror_dir,
                items: &items,
                unknown: &[],
                full: false,
                retry_failed: false,
            })
            .unwrap();
        assert_eq!(open.counts.process, 1);
        std::fs::create_dir_all(out_file.parent().unwrap()).unwrap();
        std::fs::write(&out_file, &rendered_v1.content).unwrap();
        stage
            .commit(
                "codex/s1",
                "v1",
                &rendered_v1.relative_path,
                rendered_v1.content.len() as u64,
                &rendered_v1.sha256_hex,
            )
            .unwrap();
        let fin = stage.finish().unwrap();
        assert_eq!(fin.committed, 1);
    }

    let sync_v1 = cfg.sync(false, true).unwrap();
    assert!(sync_v1.chunks_added >= 1);
    assert!(sync_v1.sparse_encoded >= 1);

    let hits_v1 = cfg
        .query("kure_sparse_sentinel_alpha", QueryMode::Sparse, 5)
        .unwrap();
    assert!(!hits_v1.is_empty(), "expected SPLADE sparse hit on v1");
    assert_eq!(hits_v1[0].path, rendered_v1.relative_path);

    // Append a new turn -> prefix-stable update -> earlier CDC chunk sparse vector reused!
    session.turns.push(Turn {
        role: Role::User,
        text: "새로 추가된 턴에는 incremental_append_omega_token 토큰이 들어 있습니다.".to_string(),
        timestamp: Some("2026-10-15T16:00:00.000Z".to_string()),
        source_offset: SourceOffset::Lines {
            start_line: 77,
            end_line: 79,
        },
    });
    let rendered_v2 = render_mirror(&session);
    assert!(rendered_v2.content.starts_with(&rendered_v1.content));
    std::fs::write(&out_file, &rendered_v2.content).unwrap();

    let sync_v2 = cfg.sync(false, true).unwrap();
    assert!(
        sync_v2.sparse_reused >= 1 || sync_v2.chunks_updated >= 1,
        "expected prefix-stable chunk reuse on append, got {sync_v2:?}"
    );

    let hits_v2 = cfg
        .query("incremental_append_omega_token", QueryMode::Sparse, 5)
        .unwrap();
    assert!(!hits_v2.is_empty(), "expected SPLADE hit on appended turn");
    let located = locate_chunk_turns(
        &rendered_v2.content,
        &rendered_v2.turn_spans,
        &hits_v2[0].text,
    );
    assert!(
        located.iter().any(|t| t.turn_index == 3),
        "located turns should include appended turn 3, got {located:?}"
    );
}
