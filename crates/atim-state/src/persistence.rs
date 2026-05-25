use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use atim_core::error::{Error, Result};
use atim_core::session::{ServerState, ThreadBinding, WindowState};

// ── Schema (self-describing — SQLite stores its own version) ──

const SCHEMA_VERSION: i32 = 1;

const CREATE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS window_states (
    window_id   TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL DEFAULT '',
    cwd         TEXT NOT NULL DEFAULT '',
    window_name TEXT NOT NULL DEFAULT '',
    agent_type  TEXT NOT NULL DEFAULT 'claude'
);

CREATE TABLE IF NOT EXISTS thread_bindings (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id        INTEGER NOT NULL,
    thread_id      INTEGER NOT NULL,
    chat_id        INTEGER NOT NULL,
    window_id      TEXT NOT NULL,
    display_name   TEXT NOT NULL DEFAULT '',
    group_chat_id  INTEGER,
    topic_name     TEXT,
    UNIQUE(user_id, thread_id, chat_id)
);

CREATE INDEX IF NOT EXISTS idx_thread_bindings_window
    ON thread_bindings(window_id);

CREATE TABLE IF NOT EXISTS session_map (
    window_id  TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL
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
    db_path: PathBuf,
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

        let store = Self {
            db: Mutex::new(connection),
            db_path,
            atim_dir: atim_dir.to_path_buf(),
        };

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

        // 1. Migrate state.json → window_states + thread_bindings
        if let Ok(data) = tokio::fs::read_to_string(&state_json).await
            && let Ok(state) = serde_json::from_str::<ServerState>(&data)
        {
            self.import_server_state(&state).await?;
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

    async fn import_server_state(&self, state: &ServerState) -> Result<()> {
        let db = self.db.lock().await;

        let mut stmt = db
            .prepare_cached(
                "INSERT OR REPLACE INTO window_states
                    (window_id, session_id, cwd, window_name, agent_type)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| Error::State(format!("prepare window_states insert: {e}")))?;

        for (wid, ws) in &state.window_states {
            stmt.execute(params![
                wid,
                ws.session_id,
                ws.cwd,
                ws.window_name,
                ws.agent_type,
            ])
            .map_err(|e| Error::State(format!("insert window_state: {e}")))?;
        }
        drop(stmt);

        let mut stmt = db
            .prepare_cached(
                "INSERT OR REPLACE INTO thread_bindings
                    (user_id, thread_id, chat_id, window_id, display_name, group_chat_id, topic_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|e| Error::State(format!("prepare bindings insert: {e}")))?;

        for tb in &state.thread_bindings {
            stmt.execute(params![
                tb.user_id,
                tb.thread_id,
                tb.chat_id,
                tb.window_id,
                tb.display_name,
                tb.group_chat_id,
                tb.topic_name,
            ])
            .map_err(|e| Error::State(format!("insert binding: {e}")))?;
        }

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

    // ── Server state ──

    /// Load the full server state from SQLite.
    pub async fn load_state(&self) -> Result<ServerState> {
        let db = self.db.lock().await;

        let mut stmt = db
            .prepare_cached(
                "SELECT window_id, session_id, cwd, window_name, agent_type
                 FROM window_states",
            )
            .map_err(|e| Error::State(format!("prepare load window_states: {e}")))?;

        let windows: HashMap<String, WindowState> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    WindowState {
                        session_id: row.get(1)?,
                        cwd: row.get(2)?,
                        window_name: row.get(3)?,
                        agent_type: row.get(4)?,
                    },
                ))
            })
            .map_err(|e| Error::State(format!("query window_states: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut stmt = db
            .prepare_cached(
                "SELECT user_id, thread_id, chat_id, window_id, display_name, group_chat_id, topic_name
                 FROM thread_bindings ORDER BY id",
            )
            .map_err(|e| Error::State(format!("prepare load bindings: {e}")))?;

        let bindings: Vec<ThreadBinding> = stmt
            .query_map([], |row| {
                Ok(ThreadBinding {
                    user_id: row.get(0)?,
                    thread_id: row.get(1)?,
                    chat_id: row.get(2)?,
                    window_id: row.get(3)?,
                    display_name: row.get(4)?,
                    group_chat_id: row.get(5)?,
                    topic_name: row.get(6)?,
                })
            })
            .map_err(|e| Error::State(format!("query bindings: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        // window_display_names and user_window_offsets are deprecated
        // but kept as empty maps for backward compat.
        Ok(ServerState {
            window_states: windows,
            thread_bindings: bindings,
            window_display_names: HashMap::new(),
            user_window_offsets: HashMap::new(),
        })
    }

    /// Save full server state to SQLite (transactional).
    pub async fn save_state(&self, state: &ServerState) -> Result<()> {
        let db = self.db.lock().await;

        db.execute_batch("BEGIN")
            .map_err(|e| Error::State(format!("begin transaction: {e}")))?;

        let r = (|| -> std::result::Result<(), rusqlite::Error> {
            db.execute("DELETE FROM window_states", [])?;
            {
                let mut stmt = db.prepare_cached(
                    "INSERT INTO window_states
                        (window_id, session_id, cwd, window_name, agent_type)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for (wid, ws) in &state.window_states {
                    stmt.execute(params![
                        wid,
                        ws.session_id,
                        ws.cwd,
                        ws.window_name,
                        ws.agent_type,
                    ])?;
                }
            }

            db.execute("DELETE FROM thread_bindings", [])?;
            {
                let mut stmt = db.prepare_cached(
                    "INSERT INTO thread_bindings
                        (user_id, thread_id, chat_id, window_id, display_name, group_chat_id, topic_name)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for tb in &state.thread_bindings {
                    stmt.execute(params![
                        tb.user_id,
                        tb.thread_id,
                        tb.chat_id,
                        tb.window_id,
                        tb.display_name,
                        tb.group_chat_id,
                        tb.topic_name,
                    ])?;
                }
            }
            Ok(())
        })();

        match r {
            Ok(()) => {
                db.execute_batch("COMMIT")
                    .map_err(|e| Error::State(format!("commit: {e}")))?;
                let path = self.atim_dir.join("state.json");
                let data = serde_json::to_string_pretty(state)
                    .map_err(|e| Error::State(format!("serialize state mirror: {e}")))?;
                tokio::fs::write(path, data).await?;
                Ok(())
            }
            Err(e) => {
                db.execute_batch("ROLLBACK").ok();
                Err(Error::State(format!("save_state failed: {e}")))
            }
        }
    }

    /// Load thread bindings only.
    pub async fn load_bindings(&self) -> Result<Vec<ThreadBinding>> {
        let state = self.load_state().await?;
        Ok(state.thread_bindings)
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
        if map.is_empty() {
            let path = self.atim_dir.join("session_map.json");
            if path.exists()
                && let Ok(data) = tokio::fs::read_to_string(&path).await
                && let Ok(from_json) = serde_json::from_str::<HashMap<String, String>>(&data)
            {
                drop(stmt);
                drop(db);
                self.save_session_map(&from_json).await?;
                return Ok(from_json);
            }
        }
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
                let path = self.atim_dir.join("session_map.json");
                let data = serde_json::to_string_pretty(map)
                    .map_err(|e| Error::State(format!("serialize session_map mirror: {e}")))?;
                tokio::fs::write(path, data).await?;
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
        if map.is_empty() {
            let path = self.atim_dir.join("monitor_state.json");
            if path.exists()
                && let Ok(data) = tokio::fs::read_to_string(&path).await
                && let Ok(from_json) = serde_json::from_str::<HashMap<String, u64>>(&data)
            {
                drop(stmt);
                drop(db);
                self.save_monitor_offsets(&from_json).await?;
                return Ok(from_json);
            }
        }
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
                let path = self.atim_dir.join("monitor_state.json");
                let data = serde_json::to_string_pretty(offsets)
                    .map_err(|e| Error::State(format!("serialize monitor_state mirror: {e}")))?;
                tokio::fs::write(path, data).await?;
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

    // ── Config TOML persistence ──

    /// Save config values to `config.toml`.
    pub async fn save_config(&self, config: &ConfigToml) -> Result<()> {
        let atim_dir = self
            .db_path
            .parent()
            .ok_or_else(|| Error::State("no parent".into()))?;
        let bytes = toml::to_string_pretty(config)
            .map_err(|e| Error::State(format!("toml serialize: {e}")))?;
        tokio::fs::write(atim_dir.join("config.toml"), bytes.as_bytes()).await?;
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

// ── Config TOML types ──

/// Config values persisted in `~/.atim/config.toml`.
///
/// Structured with TOML table sections:
/// ```toml
/// [im]
/// backend = "feishu"
///
/// [im.feishu]
/// app_id = "..."
/// app_secret = "..."
///
/// [im.telegram]
/// token = "..."
/// allowed_users = "..."
///
/// [agent]
/// command = "claude"
///
/// [tmux]
/// session = "atim"
///
/// [monitor]
/// poll_interval = "2.0"
///
/// [display]
/// show_user_messages = "true"
/// show_tool_calls = "true"
/// show_hidden_dirs = false
///
/// [openai]
/// api_key = "..."
/// base_url = "https://api.openai.com/v1"
/// ```
///
/// Note: `atim_dir` is NOT stored here — it is set via env var `ATIM_DIR` only.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ConfigToml {
    #[serde(default)]
    pub im: ImSection,
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub tmux: TmuxSection,
    #[serde(default)]
    pub monitor: MonitorSection,
    #[serde(default)]
    pub display: DisplaySection,
    #[serde(default)]
    pub openai: OpenaiSection,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImSection {
    #[serde(default = "default_im_backend")]
    pub backend: String,
    #[serde(default)]
    pub feishu: FeishuImSection,
    #[serde(default)]
    pub telegram: TelegramImSection,
}
fn default_im_backend() -> String {
    "telegram".into()
}
impl Default for ImSection {
    fn default() -> Self {
        Self {
            backend: "telegram".into(),
            feishu: FeishuImSection::default(),
            telegram: TelegramImSection::default(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FeishuImSection {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TelegramImSection {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub allowed_users: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentSection {
    #[serde(default = "default_agent_command")]
    pub command: String,
}
fn default_agent_command() -> String {
    "claude".into()
}
impl Default for AgentSection {
    fn default() -> Self {
        Self {
            command: "claude".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmuxSection {
    #[serde(default = "default_tmux_session")]
    pub session: String,
}
fn default_tmux_session() -> String {
    "atim".into()
}
impl Default for TmuxSection {
    fn default() -> Self {
        Self {
            session: "atim".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MonitorSection {
    #[serde(default = "default_poll_interval")]
    pub poll_interval: String,
}
fn default_poll_interval() -> String {
    "2.0".into()
}
impl Default for MonitorSection {
    fn default() -> Self {
        Self {
            poll_interval: "2.0".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplaySection {
    #[serde(default = "default_true")]
    pub show_user_messages: String,
    #[serde(default = "default_true")]
    pub show_tool_calls: String,
    #[serde(default)]
    pub show_hidden_dirs: bool,
}
fn default_true() -> String {
    "true".into()
}
impl Default for DisplaySection {
    fn default() -> Self {
        Self {
            show_user_messages: "true".into(),
            show_tool_calls: "true".into(),
            show_hidden_dirs: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenaiSection {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
}
fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".into()
}
impl Default for OpenaiSection {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
        }
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
