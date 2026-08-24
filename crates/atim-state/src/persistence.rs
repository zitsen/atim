use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use atim_core::config::ConfigToml;
use atim_core::error::{Error, Result};
use atim_core::session::{ChatBinding, RuntimeState, SessionInfo, WindowBinding};

// ── Schema (self-describing — SQLite stores its own version) ──

const SCHEMA_VERSION: i32 = 4;

const CREATE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

-- V1 tables (dropped — migration complete)
DROP TABLE IF EXISTS window_states;
DROP TABLE IF EXISTS thread_bindings;
CREATE TABLE IF NOT EXISTS session_map (
    window_id  TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL
);

-- V2 tables (session-driven design)
CREATE TABLE IF NOT EXISTS sessions (
    session_id   TEXT PRIMARY KEY NOT NULL,
    cwd          TEXT NOT NULL DEFAULT '',
    agent_type   TEXT NOT NULL DEFAULT 'claude',
    created_at   TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS chat_bindings (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id        INTEGER NOT NULL,
    thread_id      INTEGER NOT NULL,
    chat_id        INTEGER NOT NULL,
    display_name   TEXT NOT NULL DEFAULT '',
    group_chat_id  INTEGER,
    topic_name     TEXT,
    session_id     TEXT NOT NULL,
    reply_at_only  INTEGER NOT NULL DEFAULT 0,
    UNIQUE(user_id, thread_id, chat_id)
);
CREATE INDEX IF NOT EXISTS idx_chat_bindings_session
    ON chat_bindings(session_id);
CREATE TABLE IF NOT EXISTS window_bindings (
    window_id   TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    cwd         TEXT NOT NULL DEFAULT '',
    agent_type  TEXT NOT NULL DEFAULT 'claude',
    window_name TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS monitor_offsets (
    session_id   TEXT PRIMARY KEY NOT NULL,
    byte_offset  INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS feishu_id_map (
    id_type   TEXT NOT NULL,
    local_id  TEXT NOT NULL,
    remote_id TEXT NOT NULL,
    PRIMARY KEY (id_type, local_id)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp  TEXT NOT NULL,
    operation  TEXT NOT NULL,
    table_name TEXT NOT NULL,
    record_key TEXT NOT NULL,
    summary    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_settings (
    user_id    INTEGER NOT NULL,
    thread_id  INTEGER NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (user_id, thread_id, key)
);
";

// ── Store ──

/// SQLite-backed persistent store for all Atim state.
///
/// Provides the same public API as the old JSON-file-based `StateManager`
/// but stores everything in a single `store.db` SQLite file with proper
/// transactions, indexes, and atomic writes.
///
/// Auto-migrates from the old JSON files on first open if `store.db` does
/// not yet exist but JSON files are present.
pub struct Store {
    db: Mutex<Connection>,
    atim_dir: PathBuf,
}

impl Store {
    /// Open (or create and migrate) the store at `atim_dir`.
    ///
    /// If `store.db` doesn't exist but old JSON files are present,
    /// automatically imports their data and renames them to `.bak`.
    pub async fn open(atim_dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(atim_dir).await?;

        let db_path = atim_dir.join("store.db");
        let connection = Connection::open(&db_path)
            .map_err(|e| Error::State(format!("failed to open store.db: {e}")))?;

        // Enable WAL mode for concurrent reads + busy timeout
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|e| Error::State(format!("pragma setup failed: {e}")))?;

        connection
            .execute_batch(CREATE_SCHEMA)
            .map_err(|e| Error::State(format!("schema creation failed: {e}")))?;

        // Check / set schema version
        let version: i32 = connection
            .query_row(
                "SELECT COALESCE(MIN(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if version == 0 {
            connection
                .execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![SCHEMA_VERSION],
                )
                .ok();
        }

        // V2 → V3: add reply_at_only column to chat_bindings
        if version > 0 && version < 3 {
            connection
                .execute(
                    "ALTER TABLE chat_bindings ADD COLUMN reply_at_only INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .ok();
            connection
                .execute(
                    "UPDATE schema_version SET version = ?1",
                    params![SCHEMA_VERSION],
                )
                .ok();
        }

        // V3 → V4: chat_settings table (created by CREATE TABLE IF NOT EXISTS above)
        if version > 0 && version < 4 {
            connection
                .execute(
                    "UPDATE schema_version SET version = ?1",
                    params![SCHEMA_VERSION],
                )
                .ok();
        }

        let store = Self {
            db: Mutex::new(connection),
            atim_dir: atim_dir.to_path_buf(),
        };
        drop(db_path); // path is only needed during open

        // Auto-migrate from old JSON files
        store.migrate_from_json(atim_dir).await?;

        Ok(store)
    }

    /// Auto-import from old JSON files if they exist and store.db is fresh.
    async fn migrate_from_json(&self, atim_dir: &Path) -> Result<()> {
        let state_json = atim_dir.join("state.json");
        if !state_json.exists() {
            return Ok(());
        }

        {
            let db = self.db.lock().await;
            let count: i64 = db
                .query_row("SELECT COUNT(*) FROM window_states", [], |row| row.get(0))
                .unwrap_or(0);
            if count > 0 {
                return Ok(());
            }
        }

        tracing::info!("Migrating from JSON files to store.db...");

        // 1. Migrate state.json → V2 tables (sessions + window_bindings + chat_bindings)
        if let Ok(data) = tokio::fs::read_to_string(&state_json).await
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&data)
        {
            let db = self.db.lock().await;

            // Populate sessions and window_bindings from window_states
            if let Some(windows) = val.get("window_states").and_then(|v| v.as_object()) {
                for (wid, ws) in windows {
                    let session_id = ws.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
                    let cwd = ws.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                    let window_name = ws.get("window_name").and_then(|v| v.as_str()).unwrap_or("");
                    let agent_type = ws
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude");

                    if !session_id.is_empty() {
                        let _ = db.execute(
                            "INSERT OR IGNORE INTO sessions (session_id, cwd, agent_type, created_at)
                             VALUES (?1, ?2, ?3, datetime('now'))",
                            params![session_id, cwd, agent_type],
                        );
                    }
                    let _ = db.execute(
                        "INSERT OR IGNORE INTO window_bindings
                            (window_id, session_id, cwd, agent_type, window_name)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![wid, session_id, cwd, agent_type, window_name],
                    );
                }
            }

            // Populate chat_bindings from thread_bindings
            if let Some(bindings) = val.get("thread_bindings").and_then(|v| v.as_array()) {
                for tb in bindings {
                    let user_id = tb.get("user_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let thread_id = tb.get("thread_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let chat_id = tb.get("chat_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let window_id = tb.get("window_id").and_then(|v| v.as_str()).unwrap_or("");
                    let display_name = tb
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let group_chat_id = tb.get("group_chat_id").and_then(|v| v.as_i64());
                    let topic_name = tb.get("topic_name").and_then(|v| v.as_str());

                    // Look up session_id from the window_states in the same JSON
                    let session_id = val
                        .get("window_states")
                        .and_then(|v| v.as_object())
                        .and_then(|wins| wins.get(window_id))
                        .and_then(|ws| ws.get("session_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let _ = db.execute(
                        "INSERT OR IGNORE INTO chat_bindings
                            (user_id, thread_id, chat_id, display_name, group_chat_id, topic_name, session_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![user_id, thread_id, chat_id, display_name, group_chat_id, topic_name, session_id],
                    );
                }
            }
            drop(db);
        }

        // 2. Migrate session_map.json → session_map
        let sm_json = atim_dir.join("session_map.json");
        if sm_json.exists()
            && let Ok(data) = tokio::fs::read_to_string(&sm_json).await
            && let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&data)
        {
            self.import_session_map(&map).await?;
        }

        // 3. Migrate monitor_state.json → monitor_offsets
        let ms_json = atim_dir.join("monitor_state.json");
        if ms_json.exists()
            && let Ok(data) = tokio::fs::read_to_string(&ms_json).await
            && let Ok(map) = serde_json::from_str::<HashMap<String, u64>>(&data)
        {
            self.import_monitor_offsets(&map).await?;
        }

        // 4. Migrate feishu_id_map.json → feishu_id_map
        let fi_json = atim_dir.join("feishu_id_map.json");
        if fi_json.exists()
            && let Ok(data) = tokio::fs::read_to_string(&fi_json).await
            && let Ok(map) = serde_json::from_str::<FeishuIdMapFile>(&data)
        {
            self.import_feishu_id_map(&map).await?;
        }

        // 5. Rename old files to .bak
        for fname in &[
            "state.json",
            "session_map.json",
            "monitor_state.json",
            "feishu_id_map.json",
        ] {
            let fpath = atim_dir.join(fname);
            let bak = atim_dir.join(format!("{fname}.bak"));
            if fpath.exists() {
                tokio::fs::rename(&fpath, &bak).await.ok();
                tracing::info!("Archived {fname} → {fname}.bak");
            }
        }

        tracing::info!("Migration complete.");
        Ok(())
    }

    async fn import_session_map(&self, map: &HashMap<String, String>) -> Result<()> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached(
                "INSERT OR REPLACE INTO session_map (window_id, session_id) VALUES (?1, ?2)",
            )
            .map_err(|e| Error::State(format!("prepare session_map insert: {e}")))?;

        for (wid, sid) in map {
            stmt.execute(params![wid, sid])
                .map_err(|e| Error::State(format!("insert session_map: {e}")))?;
        }
        Ok(())
    }

    async fn import_monitor_offsets(&self, offsets: &HashMap<String, u64>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached(
                "INSERT OR REPLACE INTO monitor_offsets (session_id, byte_offset, updated_at)
                 VALUES (?1, ?2, ?3)",
            )
            .map_err(|e| Error::State(format!("prepare monitor_offsets insert: {e}")))?;

        for (sid, offset) in offsets {
            stmt.execute(params![sid, offset, now])
                .map_err(|e| Error::State(format!("insert monitor_offset: {e}")))?;
        }
        Ok(())
    }

    async fn import_feishu_id_map(&self, map: &FeishuIdMapFile) -> Result<()> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached(
                "INSERT OR REPLACE INTO feishu_id_map (id_type, local_id, remote_id)
                 VALUES (?1, ?2, ?3)",
            )
            .map_err(|e| Error::State(format!("prepare feishu map insert: {e}")))?;

        for (local, remote) in &map.chat_ids {
            stmt.execute(params!["chat", local, remote])
                .map_err(|e| Error::State(format!("insert feishu chat_id: {e}")))?;
        }
        for (local, remote) in &map.thread_ids {
            stmt.execute(params!["thread", local, remote])
                .map_err(|e| Error::State(format!("insert feishu thread_id: {e}")))?;
        }
        for (local, remote) in &map.user_ids {
            stmt.execute(params!["user", local, remote])
                .map_err(|e| Error::State(format!("insert feishu user_id: {e}")))?;
        }
        Ok(())
    }

    // ── Session map ──

    pub async fn load_session_map(&self) -> Result<HashMap<String, String>> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("SELECT window_id, session_id FROM session_map")
            .map_err(|e| Error::State(format!("prepare load session_map: {e}")))?;

        let map: HashMap<String, String> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| Error::State(format!("query session_map: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(map)
    }

    pub async fn save_session_map(&self, map: &HashMap<String, String>) -> Result<()> {
        let db = self.db.lock().await;
        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            db.execute("DELETE FROM session_map", [])?;
            let mut stmt = db.prepare_cached(
                "INSERT INTO session_map (window_id, session_id) VALUES (?1, ?2)",
            )?;
            for (wid, sid) in map {
                stmt.execute(params![wid, sid])?;
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("save_session_map failed: {e}")))
            }
        }
    }

    pub async fn clean_session_map<F>(&self, is_alive: F) -> Result<()>
    where
        F: Fn(&str) -> bool,
    {
        let map = self.load_session_map().await?;
        let before = map.len();
        let filtered: HashMap<_, _> = map.into_iter().filter(|(wid, _)| is_alive(wid)).collect();
        if filtered.len() != before {
            self.save_session_map(&filtered).await?;
        }
        Ok(())
    }

    // ── Monitor offsets ──

    pub async fn load_monitor_offsets(&self) -> Result<HashMap<String, u64>> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("SELECT session_id, byte_offset FROM monitor_offsets")
            .map_err(|e| Error::State(format!("prepare load offsets: {e}")))?;

        let map: HashMap<String, u64> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })
            .map_err(|e| Error::State(format!("query offsets: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(map)
    }

    pub async fn save_monitor_offsets(&self, offsets: &HashMap<String, u64>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            db.execute("DELETE FROM monitor_offsets", [])?;
            let mut stmt = db.prepare_cached(
                "INSERT INTO monitor_offsets (session_id, byte_offset, updated_at)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (sid, offset) in offsets {
                stmt.execute(params![sid, offset, now])?;
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("save_monitor_offsets failed: {e}")))
            }
        }
    }

    // ── Single-session offset mutations (hot path — no full cycle) ──

    /// Upsert a single session's byte offset.
    pub async fn upsert_offset(&self, session_id: &str, offset: u64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO monitor_offsets (session_id, byte_offset, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE
                 SET byte_offset=excluded.byte_offset, updated_at=excluded.updated_at",
            params![session_id, offset, now],
        )
        .map_err(|e| Error::State(format!("upsert_offset: {e}")))?;
        Ok(())
    }

    /// Remove a single session's offset entry.
    pub async fn remove_offset(&self, session_id: &str) -> Result<()> {
        let db = self.db.lock().await;
        db.execute(
            "DELETE FROM monitor_offsets WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(|e| Error::State(format!("remove_offset: {e}")))?;
        Ok(())
    }

    // ── Feishu ID map ──

    pub async fn load_feishu_id_map(&self) -> Result<FeishuIdMapFile> {
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("SELECT id_type, local_id, remote_id FROM feishu_id_map")
            .map_err(|e| Error::State(format!("prepare load feishu map: {e}")))?;

        let mut map = FeishuIdMapFile {
            chat_ids: HashMap::new(),
            thread_ids: HashMap::new(),
            user_ids: HashMap::new(),
        };

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| Error::State(format!("query feishu map: {e}")))?;

        for row in rows.flatten() {
            match row.0.as_str() {
                "chat" => {
                    map.chat_ids.insert(row.1.clone(), row.2.clone());
                }
                "thread" => {
                    map.thread_ids.insert(row.1.clone(), row.2.clone());
                }
                "user" => {
                    map.user_ids.insert(row.1.clone(), row.2.clone());
                }
                _ => {}
            }
        }
        Ok(map)
    }

    pub async fn save_feishu_id_map(&self, map: &FeishuIdMapFile) -> Result<()> {
        let db = self.db.lock().await;
        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            db.execute("DELETE FROM feishu_id_map", [])?;
            let mut stmt = db.prepare_cached(
                "INSERT INTO feishu_id_map (id_type, local_id, remote_id)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (local, remote) in &map.chat_ids {
                stmt.execute(params!["chat", local, remote])?;
            }
            for (local, remote) in &map.thread_ids {
                stmt.execute(params!["thread", local, remote])?;
            }
            for (local, remote) in &map.user_ids {
                stmt.execute(params!["user", local, remote])?;
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("save_feishu_id_map failed: {e}")))
            }
        }
    }

    // ── V2 API: RuntimeState ──

    /// Load the V2 runtime state (sessions + window_bindings + chat_bindings).
    pub async fn load_runtime(&self) -> Result<RuntimeState> {
        let db = self.db.lock().await;

        // Sessions
        let mut stmt = db
            .prepare_cached("SELECT session_id, cwd, agent_type FROM sessions")
            .map_err(|e| Error::State(format!("prepare load sessions: {e}")))?;
        let sessions: HashMap<String, SessionInfo> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    SessionInfo {
                        session_id: row.get(0)?,
                        cwd: row.get(1)?,
                        agent_type: row.get(2)?,
                    },
                ))
            })
            .map_err(|e| Error::State(format!("query sessions: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        // Window bindings
        let mut stmt = db
            .prepare_cached(
                "SELECT window_id, session_id, cwd, agent_type, window_name FROM window_bindings",
            )
            .map_err(|e| Error::State(format!("prepare load window_bindings: {e}")))?;
        let window_bindings: HashMap<String, WindowBinding> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    WindowBinding {
                        window_id: row.get(0)?,
                        session_id: row.get(1)?,
                        cwd: row.get(2)?,
                        agent_type: row.get(3)?,
                        window_name: row.get(4)?,
                    },
                ))
            })
            .map_err(|e| Error::State(format!("query window_bindings: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        // Chat bindings
        let mut stmt = db
            .prepare_cached(
                "SELECT user_id, thread_id, chat_id, display_name, group_chat_id, topic_name, session_id, reply_at_only
                 FROM chat_bindings ORDER BY id",
            )
            .map_err(|e| Error::State(format!("prepare load chat_bindings: {e}")))?;
        let chat_bindings: Vec<ChatBinding> = stmt
            .query_map([], |row| {
                Ok(ChatBinding {
                    user_id: row.get(0)?,
                    thread_id: row.get(1)?,
                    chat_id: row.get(2)?,
                    display_name: row.get(3)?,
                    group_chat_id: row.get(4)?,
                    topic_name: row.get(5)?,
                    session_id: row.get(6)?,
                    reply_at_only: row.get::<_, i32>(7).unwrap_or(0) != 0,
                })
            })
            .map_err(|e| Error::State(format!("query chat_bindings: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(RuntimeState {
            sessions,
            window_bindings,
            chat_bindings,
        })
    }

    /// Save V2 runtime state (sessions + window_bindings + chat_bindings).
    pub async fn save_runtime(&self, rt: &RuntimeState) -> Result<()> {
        let db = self.db.lock().await;
        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            // Sessions
            db.execute("DELETE FROM sessions", [])?;
            {
                let mut stmt = db.prepare_cached(
                    "INSERT INTO sessions (session_id, cwd, agent_type, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for si in rt.sessions.values() {
                    stmt.execute(params![&si.session_id, &si.cwd, &si.agent_type, ""])?;
                }
            }

            // Window bindings
            db.execute("DELETE FROM window_bindings", [])?;
            {
                let mut stmt = db.prepare_cached(
                    "INSERT INTO window_bindings
                        (window_id, session_id, cwd, agent_type, window_name)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for wb in rt.window_bindings.values() {
                    stmt.execute(params![
                        &wb.window_id,
                        &wb.session_id,
                        &wb.cwd,
                        &wb.agent_type,
                        &wb.window_name,
                    ])?;
                }
            }

            // Chat bindings
            db.execute("DELETE FROM chat_bindings", [])?;
            {
                let mut stmt = db.prepare_cached(
                    "INSERT INTO chat_bindings
                        (user_id, thread_id, chat_id, display_name, group_chat_id, topic_name, session_id, reply_at_only)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for cb in &rt.chat_bindings {
                    stmt.execute(params![
                        cb.user_id,
                        cb.thread_id,
                        cb.chat_id,
                        &cb.display_name,
                        cb.group_chat_id,
                        cb.topic_name,
                        &cb.session_id,
                        cb.reply_at_only as i32,
                    ])?;
                }
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                // Write sessions.json mirror for the monitor to read.
                let sessions_path = self.atim_dir.join("sessions.json");
                if let Ok(data) = serde_json::to_string(&rt.sessions) {
                    let tmp = sessions_path.with_extension("json.tmp");
                    if tokio::fs::write(&tmp, &data).await.is_ok() {
                        let _ = tokio::fs::rename(&tmp, &sessions_path).await;
                    }
                }
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("save_runtime failed: {e}")))
            }
        }
    }

    // ── Audit log ──

    /// Write an audit log entry for a critical state change.
    /// Silently ignores errors (audit is best-effort, must not block operations).
    fn audit(&self, operation: &str, table_name: &str, record_key: &str, summary: &str) {
        // Use unwrap_or_default on lock — if poisoned, skip audit silently.
        if let Ok(db) = self.db.try_lock() {
            let _ = db.execute(
                "INSERT INTO audit_log (timestamp, operation, table_name, record_key, summary)
                 VALUES (datetime('now'), ?1, ?2, ?3, ?4)",
                params![operation, table_name, record_key, summary],
            );
        }
    }

    // ── V2 helpers: incremental mutations ──

    /// Upsert a session into the sessions table.
    pub async fn upsert_session(&self, session: &SessionInfo) -> Result<()> {
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO sessions (session_id, cwd, agent_type, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(session_id) DO UPDATE
                 SET cwd=excluded.cwd, agent_type=excluded.agent_type",
            params![&session.session_id, &session.cwd, &session.agent_type],
        )
        .map_err(|e| Error::State(format!("upsert_session: {e}")))?;
        drop(db);
        self.audit(
            "upsert",
            "sessions",
            &session.session_id,
            &format!("cwd={} agent={}", session.cwd, session.agent_type),
        );
        Ok(())
    }

    /// Upsert a window binding.
    pub async fn upsert_window_binding(&self, wb: &WindowBinding) -> Result<()> {
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO window_bindings (window_id, session_id, cwd, agent_type, window_name)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(window_id) DO UPDATE
                 SET session_id=excluded.session_id, cwd=excluded.cwd,
                     agent_type=excluded.agent_type, window_name=excluded.window_name",
            params![
                &wb.window_id,
                &wb.session_id,
                &wb.cwd,
                &wb.agent_type,
                &wb.window_name
            ],
        )
        .map_err(|e| Error::State(format!("upsert_window_binding: {e}")))?;
        drop(db);
        self.audit(
            "upsert",
            "window_bindings",
            &wb.window_id,
            &format!(
                "session={} cwd={} name={}",
                wb.session_id, wb.cwd, wb.window_name
            ),
        );
        Ok(())
    }

    /// Upsert a chat binding.
    ///
    /// Enforces the **one session → one chat** invariant: when the binding
    /// is assigned a non-empty `session_id`, that session_id is cleared from
    /// all other chat_bindings (so no two chats share the same session).
    pub async fn upsert_chat_binding(&self, cb: &ChatBinding) -> Result<()> {
        let db = self.db.lock().await;
        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            // Upsert the target binding
            db.execute(
                "INSERT INTO chat_bindings
                    (user_id, thread_id, chat_id, display_name, group_chat_id, topic_name, session_id, reply_at_only)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(user_id, thread_id, chat_id) DO UPDATE
                     SET display_name=excluded.display_name,
                         group_chat_id=excluded.group_chat_id,
                         topic_name=excluded.topic_name,
                         session_id=excluded.session_id",
                params![
                    cb.user_id, cb.thread_id, cb.chat_id,
                    &cb.display_name, cb.group_chat_id, &cb.topic_name,
                    &cb.session_id, cb.reply_at_only as i32,
                ],
            )?;

            // Enforce one-session-per-chat: clear this session_id from all
            // *other* chat_bindings (different user_id OR thread_id OR chat_id).
            if !cb.session_id.is_empty() {
                let affected = db.execute(
                    "UPDATE chat_bindings SET session_id = ''
                     WHERE session_id = ?1
                       AND NOT (user_id = ?2 AND thread_id = ?3 AND chat_id = ?4)",
                    params![&cb.session_id, cb.user_id, cb.thread_id, cb.chat_id],
                )?;
                if affected > 0 {
                    tracing::info!(
                        "Cleared session_id '{}' from {affected} other chat_binding(s) to enforce one-session-per-chat",
                        &cb.session_id,
                    );
                }
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                drop(db);
                self.audit(
                    "upsert",
                    "chat_bindings",
                    &format!("{}:{}:{}", cb.user_id, cb.thread_id, cb.chat_id),
                    &format!("display={} session={}", cb.display_name, cb.session_id),
                );
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("upsert_chat_binding: {e}")))
            }
        }
    }

    /// Remove a window binding by window_id.
    pub async fn remove_window_binding(&self, window_id: &str) -> Result<()> {
        let db = self.db.lock().await;
        db.execute(
            "DELETE FROM window_bindings WHERE window_id = ?1",
            params![window_id],
        )
        .map_err(|e| Error::State(format!("remove_window_binding: {e}")))?;
        drop(db);
        self.audit("delete", "window_bindings", window_id, "removed");
        Ok(())
    }

    /// Clear a session_id from all window bindings (session stolen by another window).
    pub async fn clear_session_from_windows(&self, session_id: &str) -> Result<()> {
        let db = self.db.lock().await;
        let affected = db
            .execute(
                "UPDATE window_bindings SET session_id = '' WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| Error::State(format!("clear_session_from_windows: {e}")))?;
        drop(db);
        if affected > 0 {
            self.audit(
                "clear",
                "window_bindings",
                session_id,
                &format!("cleared session_id from {affected} window(s)"),
            );
        }
        Ok(())
    }

    /// Load a per-chat setting value by key.
    pub async fn load_chat_setting(
        &self,
        user_id: i64,
        thread_id: i64,
        key: &str,
    ) -> Result<Option<String>> {
        let db = self.db.lock().await;
        let result = db.query_row(
            "SELECT value FROM chat_settings WHERE user_id = ?1 AND thread_id = ?2 AND key = ?3",
            params![user_id, thread_id, key],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::State(format!("load_chat_setting: {e}"))),
        }
    }

    /// Save a per-chat setting (upsert).
    pub async fn save_chat_setting(
        &self,
        user_id: i64,
        thread_id: i64,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO chat_settings (user_id, thread_id, key, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, thread_id, key) DO UPDATE SET value = excluded.value",
            params![user_id, thread_id, key, value],
        )
        .map_err(|e| Error::State(format!("save_chat_setting: {e}")))?;
        Ok(())
    }

    /// Load session_map.json (hook output) and then delete the file.
    /// Returns the map or empty if no file/parse error.
    pub async fn consume_hook_session_map(&self) -> Result<HashMap<String, String>> {
        let path = self.atim_dir.join("session_map.json");
        let map = if path.exists() {
            match tokio::fs::read_to_string(&path)
                .await
                .map(|data| serde_json::from_str::<HashMap<String, String>>(&data).ok())
            {
                Ok(Some(m)) => m,
                _ => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        // Delete the file after reading (it's a transient pipe)
        let _ = tokio::fs::remove_file(&path).await;
        Ok(map)
    }

    // ── Config TOML persistence ──

    /// Save config values to `config.toml`.
    pub async fn save_config(&self, config: &ConfigToml) -> Result<()> {
        let bytes = toml::to_string_pretty(config)
            .map_err(|e| Error::State(format!("toml serialize: {e}")))?;
        tokio::fs::write(self.atim_dir.join("config.toml"), bytes.as_bytes()).await?;
        Ok(())
    }

    /// Load config from `config.toml`.
    pub fn load_config_toml(atim_dir: &Path) -> Option<ConfigToml> {
        let path = atim_dir.join("config.toml");
        if !path.exists() {
            return None;
        }
        let data = std::fs::read_to_string(&path).ok()?;
        toml::from_str(&data).ok()
    }

    /// Ensure `config.toml` exists. If only `.env` exists, migrate it.
    /// Returns whether a migration happened.
    pub fn ensure_config_toml(atim_dir: &Path) -> Result<bool> {
        let path = atim_dir.join("config.toml");
        if path.exists() {
            return Ok(false);
        }

        let env_path = atim_dir.join(".env");
        let mut config = ConfigToml::default();

        if env_path.exists() {
            if let Ok(data) = std::fs::read_to_string(&env_path) {
                for line in data.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, val)) = line.split_once('=') {
                        let key = key.trim();
                        let val = val.trim().trim_matches('"').to_string();
                        match key {
                            "ATIM_IM_BACKEND" => config.im.backend = val,
                            "ATIM_FEISHU_APP_ID" => config.im.feishu.app_id = val,
                            "ATIM_FEISHU_APP_SECRET" => config.im.feishu.app_secret = val,
                            "ATIM_AGENT_COMMAND" => config.agent.command = val,
                            "ATIM_TMUX_SESSION" => config.tmux.session = val,
                            "ATIM_TELEGRAM_TOKEN" => config.im.telegram.token = val,
                            "ATIM_ALLOWED_USERS" => config.im.telegram.allowed_users = val,
                            "ATIM_MONITOR_POLL_INTERVAL" => config.monitor.poll_interval = val,
                            "ATIM_SHOW_USER_MESSAGES" => config.display.show_user_messages = val,
                            "ATIM_SHOW_TOOL_CALLS" => config.display.show_tool_calls = val,
                            "ATIM_SHOW_HIDDEN_DIRS" => {
                                config.display.show_hidden_dirs = val == "true"
                            }
                            "ATIM_OPENAI_API_KEY" => config.openai.api_key = val,
                            "ATIM_OPENAI_BASE_URL" => config.openai.base_url = val,
                            _ => {}
                        }
                    }
                }
            }
            let _ = std::fs::rename(&env_path, env_path.with_extension("env.bak"));
        }

        let bytes = toml::to_string_pretty(&config)
            .map_err(|e| Error::State(format!("toml serialize: {e}")))?;
        std::fs::write(&path, &bytes)
            .map_err(|e| Error::State(format!("write config.toml: {e}")))?;

        tracing::info!("Migrated .env → config.toml");
        Ok(true)
    }
}

/// Feishu ID map file format (for migration).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeishuIdMapFile {
    pub chat_ids: HashMap<String, String>,
    pub thread_ids: HashMap<String, String>,
    pub user_ids: HashMap<String, String>,
}

/// Backward compat alias.
pub type StateManager = Store;

#[cfg(test)]
mod tests {
    use super::*;
    use atim_core::session::{ChatBinding, RuntimeState, SessionInfo, WindowBinding};

    /// Open a fresh in-memory-ish store in a unique temp directory.
    async fn test_store(suffix: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("atim-persist-test-{suffix}"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Store::open(&dir).await.expect("open store")
    }

    #[tokio::test]
    async fn test_session_map_empty_by_default() {
        let store = test_store("sm-empty").await;
        let map = store.load_session_map().await.unwrap();
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_session_map_roundtrip() {
        let store = test_store("sm-rt").await;
        let mut map = HashMap::new();
        map.insert("@0".into(), "sess-aaa-111".into());
        map.insert("@1".into(), "sess-bbb-222".into());
        store.save_session_map(&map).await.unwrap();
        let loaded = store.load_session_map().await.unwrap();
        assert_eq!(loaded, map);
    }

    #[tokio::test]
    async fn test_session_map_overwrite() {
        let store = test_store("sm-ow").await;
        let mut map = HashMap::new();
        map.insert("@0".into(), "sess-old".into());
        store.save_session_map(&map).await.unwrap();

        let mut map2 = HashMap::new();
        map2.insert("@0".into(), "sess-new".into());
        store.save_session_map(&map2).await.unwrap();

        let loaded = store.load_session_map().await.unwrap();
        assert_eq!(loaded.get("@0").map(String::as_str), Some("sess-new"));
        assert_eq!(loaded.len(), 1);
    }

    #[tokio::test]
    async fn test_monitor_offsets_roundtrip() {
        let store = test_store("mo-rt").await;
        let mut offsets = HashMap::new();
        offsets.insert("sess-abc".into(), 1024_u64);
        offsets.insert("sess-def".into(), 4096_u64);
        store.save_monitor_offsets(&offsets).await.unwrap();
        let loaded = store.load_monitor_offsets().await.unwrap();
        assert_eq!(loaded, offsets);
    }

    #[tokio::test]
    async fn test_upsert_offset_insert_and_update() {
        let store = test_store("uo-iu").await;
        store.upsert_offset("my-session", 512).await.unwrap();
        assert_eq!(
            store
                .load_monitor_offsets()
                .await
                .unwrap()
                .get("my-session")
                .copied(),
            Some(512)
        );

        // Update
        store.upsert_offset("my-session", 1024).await.unwrap();
        assert_eq!(
            store
                .load_monitor_offsets()
                .await
                .unwrap()
                .get("my-session")
                .copied(),
            Some(1024)
        );
    }

    #[tokio::test]
    async fn test_remove_offset() {
        let store = test_store("ro").await;
        store.upsert_offset("sess-x", 100).await.unwrap();
        store.remove_offset("sess-x").await.unwrap();
        assert!(
            !store
                .load_monitor_offsets()
                .await
                .unwrap()
                .contains_key("sess-x")
        );
    }

    #[tokio::test]
    async fn test_runtime_state_roundtrip() {
        let store = test_store("rt-rt").await;

        let mut sessions = HashMap::new();
        sessions.insert(
            "sess-1".into(),
            SessionInfo {
                session_id: "sess-1".into(),
                cwd: "/home/user/proj".into(),
                agent_type: "claude".into(),
            },
        );
        let mut window_bindings = HashMap::new();
        window_bindings.insert(
            "@0".into(),
            WindowBinding {
                window_id: "@0".into(),
                session_id: "sess-1".into(),
                cwd: "/home/user/proj".into(),
                agent_type: "claude".into(),
                window_name: "my-project".into(),
            },
        );
        let chat_bindings = vec![ChatBinding {
            user_id: 12345,
            thread_id: 0,
            chat_id: 67890,
            display_name: "my-project".into(),
            group_chat_id: None,
            topic_name: None,
            session_id: "sess-1".into(),
            reply_at_only: false,
        }];

        let rt = RuntimeState {
            sessions,
            window_bindings,
            chat_bindings,
        };
        store.save_runtime(&rt).await.unwrap();

        let loaded = store.load_runtime().await.unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.window_bindings.len(), 1);
        assert_eq!(loaded.chat_bindings.len(), 1);
        assert_eq!(loaded.chat_bindings[0].session_id, "sess-1");
        assert_eq!(loaded.window_bindings["@0"].window_name, "my-project");
        assert_eq!(loaded.sessions["sess-1"].cwd, "/home/user/proj");
    }

    #[tokio::test]
    async fn test_runtime_save_is_transactional() {
        // Saving with multiple entries should be all-or-nothing.
        let store = test_store("rt-tx").await;

        let mut sessions = HashMap::new();
        for i in 0..5 {
            sessions.insert(
                format!("sess-{i}"),
                SessionInfo {
                    session_id: format!("sess-{i}"),
                    cwd: "/".into(),
                    agent_type: "claude".into(),
                },
            );
        }
        let rt = RuntimeState {
            sessions,
            window_bindings: HashMap::new(),
            chat_bindings: vec![],
        };
        store.save_runtime(&rt).await.unwrap();

        // Overwrite with fewer entries
        let rt2 = RuntimeState {
            sessions: HashMap::from([(
                "sess-0".into(),
                SessionInfo {
                    session_id: "sess-0".into(),
                    cwd: "/".into(),
                    agent_type: "claude".into(),
                },
            )]),
            window_bindings: HashMap::new(),
            chat_bindings: vec![],
        };
        store.save_runtime(&rt2).await.unwrap();

        let loaded = store.load_runtime().await.unwrap();
        assert_eq!(
            loaded.sessions.len(),
            1,
            "old sessions should be gone after overwrite"
        );
    }
}
