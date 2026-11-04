use crate::mirror::MirrorTurnSpan;
use crate::model::{AgentKind, ParsedSession, Role, SourceOffset};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSession {
    pub catalog_key: String,
    pub agent: AgentKind,
    pub session_id: String,
    pub source_path: String,
    pub mirror_rel_path: String,
    pub cwd: Option<String>,
    pub title: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub turn_count: usize,
    pub mirror_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogTurn {
    pub catalog_key: String,
    pub turn_index: usize,
    pub role: Role,
    pub timestamp: Option<String>,
    pub source_offset: SourceOffset,
    pub mirror_start_byte: usize,
    pub mirror_end_byte: usize,
    pub mirror_start_line: usize,
    pub mirror_end_line: usize,
    pub text: String,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create catalog dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open catalog db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let catalog = Self { conn };
        catalog.init_schema()?;
        Ok(catalog)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                catalog_key      TEXT PRIMARY KEY,
                item_key         TEXT NOT NULL UNIQUE,
                agent            TEXT NOT NULL,
                session_id       TEXT NOT NULL,
                source_path      TEXT NOT NULL,
                mirror_rel_path  TEXT NOT NULL UNIQUE,
                cwd              TEXT,
                title            TEXT NOT NULL,
                started_at       TEXT,
                ended_at         TEXT,
                turn_count       INTEGER NOT NULL,
                mirror_sha256    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_session_id ON sessions(session_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent);
            CREATE INDEX IF NOT EXISTS idx_sessions_mirror_path ON sessions(mirror_rel_path);

            CREATE TABLE IF NOT EXISTS turns (
                catalog_key        TEXT NOT NULL REFERENCES sessions(catalog_key) ON DELETE CASCADE,
                turn_index         INTEGER NOT NULL,
                role               TEXT NOT NULL,
                timestamp          TEXT,
                source_offset_json TEXT NOT NULL,
                mirror_start_byte  INTEGER NOT NULL,
                mirror_end_byte    INTEGER NOT NULL,
                mirror_start_line  INTEGER NOT NULL,
                mirror_end_line    INTEGER NOT NULL,
                text               TEXT NOT NULL,
                PRIMARY KEY (catalog_key, turn_index)
            );

            CREATE TABLE IF NOT EXISTS chunks (
                chunk_id        TEXT PRIMARY KEY,
                catalog_key     TEXT NOT NULL,
                mirror_rel_path TEXT NOT NULL,
                start_turn      INTEGER NOT NULL,
                end_turn        INTEGER NOT NULL,
                text            TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_catalog_key ON chunks(catalog_key);
            ",
        )?;
        Ok(())
    }

    pub fn upsert_session(
        &mut self,
        item_key: &str,
        session: &ParsedSession,
        mirror_rel_path: &str,
        mirror_sha256: &str,
        turn_spans: &[MirrorTurnSpan],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let catalog_key = session.catalog_key();
        let title = session.display_title();

        tx.execute(
            "
            INSERT INTO sessions (
                catalog_key, item_key, agent, session_id, source_path, mirror_rel_path,
                cwd, title, started_at, ended_at, turn_count, mirror_sha256
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(catalog_key) DO UPDATE SET
                item_key = excluded.item_key,
                source_path = excluded.source_path,
                mirror_rel_path = excluded.mirror_rel_path,
                cwd = excluded.cwd,
                title = excluded.title,
                started_at = COALESCE(sessions.started_at, excluded.started_at),
                ended_at = excluded.ended_at,
                turn_count = excluded.turn_count,
                mirror_sha256 = excluded.mirror_sha256
            ",
            params![
                catalog_key,
                item_key,
                session.agent.as_str(),
                session.session_id,
                session.source_path,
                mirror_rel_path,
                session.cwd,
                title,
                session.started_at,
                session.ended_at,
                turn_spans.len() as i64,
                mirror_sha256,
            ],
        )?;

        tx.execute(
            "DELETE FROM turns WHERE catalog_key = ?1",
            params![catalog_key],
        )?;
        {
            let mut stmt = tx.prepare(
                "
                INSERT INTO turns (
                    catalog_key, turn_index, role, timestamp, source_offset_json,
                    mirror_start_byte, mirror_end_byte, mirror_start_line, mirror_end_line, text
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ",
            )?;
            for (turn, span) in session.turns.iter().zip(turn_spans.iter()) {
                let offset_json = serde_json::to_string(&span.source_offset)?;
                stmt.execute(params![
                    catalog_key,
                    span.turn_index as i64,
                    span.role.as_str(),
                    span.timestamp,
                    offset_json,
                    span.mirror_start_byte as i64,
                    span.mirror_end_byte as i64,
                    span.mirror_start_line as i64,
                    span.mirror_end_line as i64,
                    turn.text,
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn remove_by_item_key(&mut self, item_key: &str) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM sessions WHERE item_key = ?1",
            params![item_key],
        )?;
        Ok(deleted > 0)
    }

    pub fn get_by_mirror_path(&self, mirror_rel_path: &str) -> Result<Option<CatalogSession>> {
        self.query_one_session(
            "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
             FROM sessions WHERE mirror_rel_path = ?1",
            params![mirror_rel_path],
        )
    }

    /// Resolve `<agent>:<session_id>`, exact `<session_id>`, or unique `<session_id>` prefix.
    pub fn resolve_session(&self, selector: &str) -> Result<Option<CatalogSession>> {
        if let Some(s) = self.query_one_session(
            "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
             FROM sessions WHERE catalog_key = ?1",
            params![selector],
        )? {
            return Ok(Some(s));
        }
        if let Some(s) = self.query_one_session(
            "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
             FROM sessions WHERE session_id = ?1 ORDER BY ended_at DESC LIMIT 1",
            params![selector],
        )? {
            return Ok(Some(s));
        }
        let like = format!("{selector}%");
        let mut stmt = self.conn.prepare(
            "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
             FROM sessions WHERE session_id LIKE ?1 OR catalog_key LIKE ?1 ORDER BY ended_at DESC LIMIT 2",
        )?;
        let rows: Vec<CatalogSession> = stmt
            .query_map(params![like], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() == 1 {
            Ok(rows.into_iter().next())
        } else {
            Ok(None)
        }
    }

    pub fn list_sessions(
        &self,
        agent: Option<AgentKind>,
        limit: usize,
    ) -> Result<Vec<CatalogSession>> {
        let mut out = Vec::new();
        if let Some(a) = agent {
            let mut stmt = self.conn.prepare(
                "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
                 FROM sessions WHERE agent = ?1 ORDER BY COALESCE(ended_at, started_at, '') DESC, catalog_key ASC LIMIT ?2",
            )?;
            for r in stmt.query_map(params![a.as_str(), limit as i64], map_session_row)? {
                out.push(r?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT catalog_key, agent, session_id, source_path, mirror_rel_path, cwd, title, started_at, ended_at, turn_count, mirror_sha256
                 FROM sessions ORDER BY COALESCE(ended_at, started_at, '') DESC, catalog_key ASC LIMIT ?1",
            )?;
            for r in stmt.query_map(params![limit as i64], map_session_row)? {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn count_sessions_and_turns(&self) -> Result<(usize, usize)> {
        let sessions: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        let turns: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))?;
        Ok((sessions as usize, turns as usize))
    }

    pub fn get_turns(&self, catalog_key: &str) -> Result<Vec<CatalogTurn>> {
        let mut stmt = self.conn.prepare(
            "SELECT catalog_key, turn_index, role, timestamp, source_offset_json,
                    mirror_start_byte, mirror_end_byte, mirror_start_line, mirror_end_line, text
             FROM turns WHERE catalog_key = ?1 ORDER BY turn_index ASC",
        )?;
        let rows = stmt
            .query_map(params![catalog_key], |row| {
                let role_str: String = row.get(2)?;
                let role = if role_str == "user" {
                    Role::User
                } else {
                    Role::Assistant
                };
                let offset_json: String = row.get(4)?;
                let source_offset: SourceOffset =
                    serde_json::from_str(&offset_json).unwrap_or(SourceOffset::Lines {
                        start_line: 1,
                        end_line: 1,
                    });
                Ok(CatalogTurn {
                    catalog_key: row.get(0)?,
                    turn_index: row.get::<_, i64>(1)? as usize,
                    role,
                    timestamp: row.get(3)?,
                    source_offset,
                    mirror_start_byte: row.get::<_, i64>(5)? as usize,
                    mirror_end_byte: row.get::<_, i64>(6)? as usize,
                    mirror_start_line: row.get::<_, i64>(7)? as usize,
                    mirror_end_line: row.get::<_, i64>(8)? as usize,
                    text: row.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_turn_spans(&self, catalog_key: &str) -> Result<Vec<MirrorTurnSpan>> {
        Ok(self
            .get_turns(catalog_key)?
            .into_iter()
            .map(|t| MirrorTurnSpan {
                turn_index: t.turn_index,
                role: t.role,
                timestamp: t.timestamp,
                source_offset: t.source_offset,
                mirror_start_byte: t.mirror_start_byte,
                mirror_end_byte: t.mirror_end_byte,
                mirror_start_line: t.mirror_start_line,
                mirror_end_line: t.mirror_end_line,
            })
            .collect())
    }

    pub fn record_chunk(
        &self,
        chunk_id: &str,
        catalog_key: &str,
        mirror_rel_path: &str,
        start_turn: usize,
        end_turn: usize,
        text: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO chunks (chunk_id, catalog_key, mirror_rel_path, start_turn, end_turn, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(chunk_id) DO UPDATE SET
                catalog_key = excluded.catalog_key,
                mirror_rel_path = excluded.mirror_rel_path,
                start_turn = excluded.start_turn,
                end_turn = excluded.end_turn,
                text = excluded.text",
            params![
                chunk_id,
                catalog_key,
                mirror_rel_path,
                start_turn as i64,
                end_turn as i64,
                text
            ],
        )?;
        Ok(())
    }

    pub fn get_chunk(&self, chunk_id: &str) -> Result<Option<RecordedChunk>> {
        self.conn
            .query_row(
                "SELECT chunk_id, catalog_key, mirror_rel_path, start_turn, end_turn, text
                 FROM chunks WHERE chunk_id = ?1",
                params![chunk_id],
                |row| {
                    Ok(RecordedChunk {
                        chunk_id: row.get(0)?,
                        catalog_key: row.get(1)?,
                        mirror_rel_path: row.get(2)?,
                        start_turn: row.get::<_, i64>(3)? as usize,
                        end_turn: row.get::<_, i64>(4)? as usize,
                        text: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_mirror_sha256(&self, item_key: &str) -> Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT mirror_rel_path, mirror_sha256 FROM sessions WHERE item_key = ?1",
                params![item_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_item_key(&self, catalog_key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT item_key FROM sessions WHERE catalog_key = ?1",
                params![catalog_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn query_one_session(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<CatalogSession>> {
        self.conn
            .query_row(sql, params, map_session_row)
            .optional()
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedChunk {
    pub chunk_id: String,
    pub catalog_key: String,
    pub mirror_rel_path: String,
    pub start_turn: usize,
    pub end_turn: usize,
    pub text: String,
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CatalogSession> {
    let agent_str: String = row.get(1)?;
    let agent = AgentKind::parse_name(&agent_str).unwrap_or(AgentKind::Codex);
    Ok(CatalogSession {
        catalog_key: row.get(0)?,
        agent,
        session_id: row.get(2)?,
        source_path: row.get(3)?,
        mirror_rel_path: row.get(4)?,
        cwd: row.get(5)?,
        title: row.get(6)?,
        started_at: row.get(7)?,
        ended_at: row.get(8)?,
        turn_count: row.get::<_, i64>(9)? as usize,
        mirror_sha256: row.get(10)?,
    })
}
