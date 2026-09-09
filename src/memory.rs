//! SQLite-backed conversation memory store.
//!
//! Stores all conversations by date and provides summarization capabilities.
//! On new sessions, recent summaries are injected as context.

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub id: i64,
    pub date: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryEntry {
    pub id: i64,
    pub date: String,
    pub summary: String,
    pub created_at: String,
}

/// A checkpoint of an in-progress agent task, persisted to SQLite
/// so it can be resumed after a crash or restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    pub id: String,
    pub session_id: String,
    pub model_name: String,
    pub user_message: String,
    /// Full conversation history serialized as JSON array of ChatMessage.
    pub history_json: String,
    /// Iteration number when the checkpoint was saved.
    pub iteration: usize,
    /// Human-readable summary of tools used (e.g. "bash, read_file (3 rounds)").
    pub tool_summary: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MemoryStore {
    conn: Mutex<Connection>,
}

fn compose_summary_from_entries(entries: &[ConversationEntry]) -> Result<String, String> {
    if entries.is_empty() {
        return Err("No conversations to summarize".to_string());
    }

    let mut parts: Vec<String> = Vec::new();

    let user_msgs: Vec<String> = entries.iter()
        .filter(|e| e.role == "user")
        .map(|e| e.content.chars().take(150).collect::<String>())
        .collect();
    if !user_msgs.is_empty() {
        parts.push(format!("User questions/topics ({}):", user_msgs.len()));
        for (i, m) in user_msgs.iter().take(30).enumerate() {
            parts.push(format!("  {}. {}", i + 1, m));
        }
    }

    let asst_msgs: Vec<String> = entries.iter()
        .filter(|e| e.role == "assistant")
        .map(|e| e.content.chars().take(200).collect::<String>())
        .collect();
    if !asst_msgs.is_empty() {
        parts.push(format!("\nAssistant responses ({}):", asst_msgs.len()));
        for (i, m) in asst_msgs.iter().take(15).enumerate() {
            parts.push(format!("  {}. {}", i + 1, m));
        }
    }

    if parts.is_empty() {
        return Err("No user/assistant entries to summarize".to_string());
    }

    let mut summary = parts.join("\n");
    if summary.len() > 4000 {
        summary = format!(
            "{}\n\n... [summary truncated]",
            summary.chars().take(4000).collect::<String>()
        );
    }
    Ok(summary)
}

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{f900}'..='\u{faff}'
        | '\u{2e80}'..='\u{2eff}'
        | '\u{3000}'..='\u{303f}'
        | '\u{3040}'..='\u{309f}'
        | '\u{30a0}'..='\u{30ff}'
        | '\u{ac00}'..='\u{d7af}'
    )
}

/// Insert spaces between consecutive CJK characters so that the FTS5
/// `unicode61` tokenizer treats each CJK character as an individual token.
/// Latin words are left untouched (their existing whitespace is preserved).
fn preprocess_for_fts(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + text.len() / 4);
    let mut prev_cjk = false;
    for ch in text.chars() {
        if is_cjk_char(ch) {
            if prev_cjk {
                result.push(' ');
            }
            result.push(ch);
            prev_cjk = true;
        } else {
            if prev_cjk {
                result.push(' ');
            }
            result.push(ch);
            prev_cjk = false;
        }
    }
    result
}

/// Build FTS5 query tokens from a user query string.
///
/// - CJK segments → 2-character phrase queries (e.g. `"安 全"`)
/// - Latin words → quoted exact-match tokens (e.g. `"security"`)
///
/// All tokens are returned ready to be joined with ` OR ` for the FTS5
/// MATCH expression.
fn build_fts_query_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    for word in query.split_whitespace() {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let has_cjk = chars.iter().copied().any(is_cjk_char);

        if has_cjk {
            // Generate CJK bigram phrase queries.
            // Each bigram becomes `"X Y"` which FTS5 matches as adjacent tokens.
            for window in chars.windows(2) {
                let c0 = window[0];
                let c1 = window[1];
                if is_cjk_char(c0) || is_cjk_char(c1) {
                    // Escape any double-quotes inside the token
                    let s = format!("{} {}", c0, c1);
                    tokens.push(format!("\"{}\"", s.replace('"', "\"\"")));
                }
            }
            // Also add the full CJK string as individual character tokens OR'd
            // so that single-char queries still match something.
            if chars.len() == 1 && is_cjk_char(chars[0]) {
                let escaped = format!("{}", chars[0]).replace('"', "\"\"");
                tokens.push(format!("\"{}\"", escaped));
            }
        } else {
            // Latin word — quote it for exact token match
            let escaped = lower.replace('"', "\"\"");
            tokens.push(format!("\"{}\"", escaped));
        }
    }

    // Fallback: if the query had no whitespace (single CJK string),
    // generate bigrams from the whole string.
    if tokens.is_empty() {
        let lower = query.trim().to_lowercase();
        if !lower.is_empty() {
            let chars: Vec<char> = lower.chars().collect();
            if chars.iter().copied().any(is_cjk_char) {
                for window in chars.windows(2) {
                    let c0 = window[0];
                    let c1 = window[1];
                    if is_cjk_char(c0) || is_cjk_char(c1) {
                        let s = format!("{} {}", c0, c1);
                        tokens.push(format!("\"{}\"", s.replace('"', "\"\"")));
                    }
                }
            }
            if tokens.is_empty() {
                let escaped = lower.replace('"', "\"\"");
                tokens.push(format!("\"{}\"", escaped));
            }
        }
    }

    tokens
}

/// Legacy keyword extraction — kept for `is_recall_query()` in server.rs.
/// The FTS5 search path uses `build_fts_query_tokens` instead.
fn extract_search_keywords(query: &str) -> Vec<String> {
    let mut keywords = Vec::new();

    for token in query.split_whitespace() {
        if token.is_empty() {
            continue;
        }
        let lower = token.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let has_cjk = chars.iter().copied().any(is_cjk_char);

        if has_cjk {
            for window in chars.windows(2) {
                let bigram: String = window.iter().collect();
                if bigram.chars().any(is_cjk_char) {
                    keywords.push(bigram);
                }
            }
            keywords.push(lower);
        } else {
            keywords.push(lower);
        }
    }

    if keywords.is_empty() {
        let lower = query.trim().to_lowercase();
        if !lower.is_empty() {
            let chars: Vec<char> = lower.chars().collect();
            if chars.iter().copied().any(is_cjk_char) {
                for window in chars.windows(2) {
                    let bigram: String = window.iter().collect();
                    if bigram.chars().any(is_cjk_char) {
                        keywords.push(bigram);
                    }
                }
            }
            keywords.push(lower);
        }
    }

    keywords.sort();
    keywords.dedup();
    keywords
}

impl MemoryStore {
    /// Open or create the SQLite database at the given path.
    pub fn new(db_path: &str) -> Result<Self, String> {
        let path = PathBuf::from(db_path);
        let conn = Connection::open(&path)
            .map_err(|e| format!("Failed to open memory DB: {}", e))?;

        // Enable WAL mode for better concurrent performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("Failed to set WAL mode: {}", e))?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        store.ensure_two_tier_schema()?;
        info!("Memory store initialized: {}", db_path);
        Ok(store)
    }

    /// Run schema migrations.
    fn migrate(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                date TEXT NOT NULL,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_name TEXT,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_conv_date ON conversations(date);
            CREATE INDEX IF NOT EXISTS idx_conv_session ON conversations(session_id);

            CREATE TABLE IF NOT EXISTS summaries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                date TEXT NOT NULL UNIQUE,
                summary TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );"
        ).map_err(|e| format!("Migration failed: {}", e))?;

        // Insert version if not exists
        let version: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        ).unwrap_or(0);

        if version < 1 {
            conn.execute("INSERT OR REPLACE INTO schema_version (version) VALUES (1)", [])
                .map_err(|e| format!("Version insert failed: {}", e))?;
        }

        // ── Schema v2: FTS5 full-text search index ──────────────
        if version < 2 {
            // Standalone FTS5 table (not content-synced, because we need
            // CJK preprocessing before indexing — triggers can't call Rust).
            conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS conversations_fts USING fts5(
                    content,
                    tokenize='unicode61 remove_diacritics 0'
                );"
            ).map_err(|e| format!("FTS5 table creation failed: {}", e))?;

            // Backfill existing conversations into the FTS index.
            let rows: Vec<(i64, String)> = conn.prepare(
                "SELECT rowid, content FROM conversations"
            )
            .map_err(|e| format!("FTS backfill query failed: {}", e))?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| format!("FTS backfill query_map failed: {}", e))?
            .filter_map(|r| r.ok())
            .collect();

            for (rowid, content) in &rows {
                let fts_content = preprocess_for_fts(content);
                conn.execute(
                    "INSERT INTO conversations_fts(rowid, content) VALUES (?1, ?2)",
                    params![rowid, fts_content],
                ).map_err(|e| format!("FTS backfill insert failed: {}", e))?;
            }

            conn.execute("INSERT INTO schema_version(version) VALUES(2)", [])
                .map_err(|e| format!("Version 2 insert failed: {}", e))?;

            info!("Schema v2 migration: FTS5 index created ({} entries indexed)", rows.len());
        }

        // ── Schema v3: Task checkpoints for crash recovery ──────
        if version < 3 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS task_checkpoints (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    model_name TEXT NOT NULL,
                    user_message TEXT NOT NULL,
                    history_json TEXT NOT NULL,
                    iteration INTEGER NOT NULL DEFAULT 0,
                    tool_summary TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );"
            ).map_err(|e| format!("Checkpoint table creation failed: {}", e))?;

            conn.execute("INSERT INTO schema_version(version) VALUES(3)", [])
                .map_err(|e| format!("Version 3 insert failed: {}", e))?;

            info!("Schema v3 migration: task_checkpoints table created");
        }

        // ── Schema v4: Token usage tracking ─────────────────────
        if version < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS usage_stats (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp TEXT NOT NULL,
                    model_name TEXT NOT NULL,
                    session_id TEXT,
                    prompt_tokens INTEGER NOT NULL DEFAULT 0,
                    completion_tokens INTEGER NOT NULL DEFAULT 0,
                    total_tokens INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_stats(timestamp);
                CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_stats(model_name);"
            ).map_err(|e| format!("Usage stats table creation failed: {}", e))?;

            conn.execute("INSERT INTO schema_version(version) VALUES(4)", [])
                .map_err(|e| format!("Version 4 insert failed: {}", e))?;

            info!("Schema v4 migration: usage_stats table created");
        }

        // ── Schema v5: TaskContracts for managed (long-horizon) tasks ──
        if version < 5 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS task_contracts (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    contract_json TEXT NOT NULL,
                    phase TEXT NOT NULL DEFAULT 'collection',
                    current_round INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_contracts_session ON task_contracts(session_id);"
            ).map_err(|e| format!("Task contracts table creation failed: {}", e))?;

            conn.execute("INSERT INTO schema_version(version) VALUES(5)", [])
                .map_err(|e| format!("Version 5 insert failed: {}", e))?;

            info!("Schema v5 migration: task_contracts table created");
        }

        // ── Schema v7: blocked_reason column for task_contracts ──
        // Used to mark contracts as user-stopped ('[USER_STOPPED]') so the resume
        // query can find them even if they are in 'blocked' phase.
        if version < 7 {
            conn.execute_batch(
                "ALTER TABLE task_contracts ADD COLUMN blocked_reason TEXT;"
            ).map_err(|e| format!("v7 migration failed: {}", e))?;

            conn.execute("INSERT INTO schema_version(version) VALUES(7)", [])
                .map_err(|e| format!("Version 7 insert failed: {}", e))?;

            info!("Schema v7 migration: blocked_reason column added to task_contracts");
        }

        Ok(())
    }

    /// Store a conversation entry and update the FTS5 index.
    pub fn store_entry(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tool_name: Option<&str>,
    ) -> Result<i64, String> {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let timestamp = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (date, session_id, role, content, tool_name, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![date, session_id, role, content, tool_name, timestamp],
        ).map_err(|e| format!("Failed to store entry: {}", e))?;
        let rowid = conn.last_insert_rowid();

        // Also insert into FTS5 index with CJK-preprocessed content.
        // FTS5 table may not exist on very old DBs that haven't migrated yet;
        // silently ignore that error.
        let fts_content = preprocess_for_fts(content);
        let _ = conn.execute(
            "INSERT INTO conversations_fts(rowid, content) VALUES (?1, ?2)",
            params![rowid, fts_content],
        );

        Ok(rowid)
    }

    /// Get all conversation entries for a specific date.
    pub fn get_entries_by_date(&self, date: &str) -> Result<Vec<ConversationEntry>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, date, session_id, role, content, tool_name, timestamp FROM conversations WHERE date = ?1 ORDER BY timestamp ASC"
        ).map_err(|e| format!("Query prepare failed: {}", e))?;

        let entries = stmt.query_map(params![date], |row| {
            Ok(ConversationEntry {
                id: row.get(0)?,
                date: row.get(1)?,
                session_id: row.get(2)?,
                role: row.get(3)?,
                content: row.get(4)?,
                tool_name: row.get(5)?,
                timestamp: row.get(6)?,
            })
        }).map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(entries)
    }

    /// Get recent entries from the last N days.
    pub fn get_recent_entries(&self, days: usize) -> Result<Vec<ConversationEntry>, String> {
        let since = (Utc::now() - chrono::Duration::days(days as i64))
            .format("%Y-%m-%d")
            .to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, date, session_id, role, content, tool_name, timestamp FROM conversations WHERE date >= ?1 ORDER BY timestamp ASC"
        ).map_err(|e| format!("Query prepare failed: {}", e))?;

        let entries = stmt.query_map(params![since], |row| {
            Ok(ConversationEntry {
                id: row.get(0)?,
                date: row.get(1)?,
                session_id: row.get(2)?,
                role: row.get(3)?,
                content: row.get(4)?,
                tool_name: row.get(5)?,
                timestamp: row.get(6)?,
            })
        }).map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(entries)
    }

    /// Full-text search across recent conversation entries using FTS5.
    ///
    /// Uses BM25 ranking so the most relevant results come first.
    /// CJK text is handled via bigram phrase queries.
    /// Falls back to the legacy linear scan if the FTS5 table is missing.
    pub fn search_entries(&self, query: &str, days: usize) -> Result<Vec<ConversationEntry>, String> {
        let tokens = build_fts_query_tokens(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let fts_query = tokens.join(" OR ");
        let since = (Utc::now() - chrono::Duration::days(days as i64))
            .format("%Y-%m-%d")
            .to_string();

        let conn = self.conn.lock().unwrap();

        // Try FTS5 search first.
        let fts_result = (|| -> Result<Vec<ConversationEntry>, String> {
            let mut stmt = conn.prepare(
                "SELECT c.id, c.date, c.session_id, c.role, c.content, c.tool_name, c.timestamp
                 FROM conversations_fts f
                 JOIN conversations c ON c.rowid = f.rowid
                 WHERE conversations_fts MATCH ?1
                   AND c.date >= ?2
                 ORDER BY bm25(conversations_fts)
                 LIMIT 30"
            ).map_err(|e| format!("FTS query prepare failed: {}", e))?;

            let entries = stmt.query_map(params![fts_query, since], |row| {
                Ok(ConversationEntry {
                    id: row.get(0)?,
                    date: row.get(1)?,
                    session_id: row.get(2)?,
                    role: row.get(3)?,
                    content: row.get(4)?,
                    tool_name: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            }).map_err(|e| format!("FTS query failed: {}", e))?
            .filter_map(|r| r.ok())
            .collect();

            Ok(entries)
        })();

        match fts_result {
            Ok(entries) => Ok(entries),
            Err(e) => {
                // FTS5 table might not exist (e.g. migration not yet run).
                // Fall back to legacy linear scan.
                warn!("FTS5 search failed ({}), falling back to linear scan", e);
                self.search_entries_legacy(query, days)
            }
        }
    }

    /// Legacy keyword search (fallback when FTS5 is unavailable).
    fn search_entries_legacy(&self, query: &str, days: usize) -> Result<Vec<ConversationEntry>, String> {
        let keywords = extract_search_keywords(query);
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        let recent = self.get_recent_entries(days)?;
        let mut matched: Vec<ConversationEntry> = Vec::new();
        for entry in recent {
            let content_lower = entry.content.to_lowercase();
            if keywords.iter().any(|kw| content_lower.contains(kw)) {
                matched.push(entry);
            }
        }
        if matched.len() > 30 {
            let start = matched.len() - 30;
            matched = matched.split_off(start);
        }
        Ok(matched)
    }

    /// Build a recall context for a user query by searching SQLite and
    /// summarizing the matching entries. Used when the user asks about past
    /// conversations mid-session.
    pub fn build_recall_context(&self, query: &str, days: usize) -> Option<String> {
        // Ensure daily summaries exist for an overview.
        self.ensure_recent_summaries(days);

        let mut parts: Vec<String> = Vec::new();

        // 1. Keyword-matched entries from the last N days.
        if let Ok(hits) = self.search_entries(query, days) {
            if !hits.is_empty() {
                parts.push(format!("## Relevant past messages matching \"{}\" ({} hits)", query, hits.len()));
                for e in hits.iter().take(20) {
                    let role_label = match e.role.as_str() {
                        "user" => "User",
                        "assistant" => "Assistant",
                        _ => "System",
                    };
                    let preview: String = e.content.chars().take(300).collect();
                    let suffix = if e.content.chars().count() > 300 { "..." } else { "" };
                    let when = chrono::DateTime::parse_from_rfc3339(&e.timestamp)
                        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|_| e.date.clone());
                    parts.push(format!("[{}] {}: {}{}", when, role_label, preview, suffix));
                }
            }
        }

        // 2. Daily summaries for broader context.
        let mut added_summary_section = false;
        if let Ok(summaries) = self.get_recent_summaries(days) {
            if !summaries.is_empty() {
                added_summary_section = true;
                parts.push("\n## Daily conversation summaries".to_string());
                for s in &summaries {
                    parts.push(format!("\n### {}", s.date));
                    parts.push(s.summary.clone());
                }
            }
        }
        if !added_summary_section {
            if let Ok(dates) = self.available_dates() {
                let mut generated = Vec::new();
                for date in dates.into_iter().take(days) {
                    if let Ok(entries) = self.get_entries_by_date(&date) {
                        if let Ok(summary) = compose_summary_from_entries(&entries) {
                            generated.push((date, summary));
                        }
                    }
                }
                if !generated.is_empty() {
                    parts.push("\n## Daily conversation summaries".to_string());
                    for (date, summary) in generated {
                        parts.push(format!("\n### {}", date));
                        parts.push(summary);
                    }
                }
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(format!(
                "[Memory Recall — the user is asking about earlier conversations. \
                 Below are relevant past messages and daily summaries retrieved from \
                 the local memory store. Use them to answer; do NOT claim you have no \
                 memory of past conversations when this block is present.]\n\n{}",
                parts.join("\n")
            ))
        }
    }

    /// Store a summary for a date (upsert).
    pub fn store_summary(&self, date: &str, summary: &str) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO summaries (date, summary, created_at) VALUES (?1, ?2, ?3)",
            params![date, summary, now],
        ).map_err(|e| format!("Failed to store summary: {}", e))?;
        Ok(())
    }

    /// Get recent summaries (last N days).
    pub fn get_recent_summaries(&self, days: usize) -> Result<Vec<SummaryEntry>, String> {
        let since = (Utc::now() - chrono::Duration::days(days as i64))
            .format("%Y-%m-%d")
            .to_string();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, date, summary, created_at FROM summaries WHERE date >= ?1 ORDER BY date DESC"
        ).map_err(|e| format!("Query prepare failed: {}", e))?;

        let entries = stmt.query_map(params![since], |row| {
            Ok(SummaryEntry {
                id: row.get(0)?,
                date: row.get(1)?,
                summary: row.get(2)?,
                created_at: row.get(3)?,
            })
        }).map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(entries)
    }

    /// Get all available dates with conversations.
    pub fn available_dates(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT date FROM conversations ORDER BY date DESC"
        ).map_err(|e| format!("Query prepare failed: {}", e))?;

        let dates = stmt.query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("Query failed: {}", e))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(dates)
    }

    /// Get all summaries.
    pub fn get_all_summaries(&self) -> Result<Vec<SummaryEntry>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, date, summary, created_at FROM summaries ORDER BY date DESC"
        ).map_err(|e| format!("Query prepare failed: {}", e))?;

        let entries = stmt.query_map([], |row| {
            Ok(SummaryEntry {
                id: row.get(0)?,
                date: row.get(1)?,
                summary: row.get(2)?,
                created_at: row.get(3)?,
            })
        }).map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(entries)
    }

    /// Get the summary for a single date (if it exists).
    pub fn get_summary_for_date(&self, date: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT summary FROM summaries WHERE date = ?1",
            params![date],
            |row| row.get(0),
        );
        result.ok()
    }

    /// Auto-generate an extractive summary for a date's conversations and
    /// persist it (upsert). Returns the generated summary text.
    ///
    /// This is intentionally extractive (no LLM call) so it is cheap to run
    /// after every chat and keeps the daily summary fresh.
    pub fn auto_summarize_date(&self, date: &str) -> Result<String, String> {
        let entries = self.get_entries_by_date(date)?;
        if entries.is_empty() {
            return Err(format!("No conversations for {}", date));
        }
        let summary = compose_summary_from_entries(&entries)?;

        self.store_summary(date, &summary)?;
        Ok(summary)
    }

    /// Ensure summaries exist for the last N days that have conversations.
    /// Today's summary is always refreshed (to capture the latest entries);
    /// past days are generated once.
    pub fn ensure_recent_summaries(&self, days: usize) {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let dates = match self.available_dates() {
            Ok(d) => d,
            Err(_) => return,
        };
        // `available_dates` returns DESC order, so `take(days)` gives the most
        // recent days that actually have conversations.
        for date in dates.into_iter().take(days) {
            if date == today {
                let _ = self.auto_summarize_date(&date);
            } else if self.get_summary_for_date(&date).is_none() {
                let _ = self.auto_summarize_date(&date);
            }
        }
    }

    /// Build a memory context string from recent daily summaries.
    /// This is injected as context so the agent can recall earlier
    /// conversations when the user asks about past topics.
    pub fn build_context_string(&self, summary_days: usize) -> Option<String> {
        // Lazily ensure summaries exist (and today's is fresh) before reading.
        self.ensure_recent_summaries(summary_days);

        let mut parts = Vec::new();

        // Recent daily summaries (includes today if it has conversations).
        // If the summaries table is empty or stale, fall back to synthesizing
        // summaries directly from raw conversation entries.
        let mut added = false;
        if let Ok(summaries) = self.get_recent_summaries(summary_days) {
            if !summaries.is_empty() {
                added = true;
                parts.push("## Past Conversation Summaries".to_string());
                for s in &summaries {
                    parts.push(format!("\n### {}", s.date));
                    parts.push(s.summary.clone());
                }
            }
        }
        if !added {
            if let Ok(dates) = self.available_dates() {
                let mut generated = Vec::new();
                for date in dates.into_iter().take(summary_days) {
                    if let Ok(entries) = self.get_entries_by_date(&date) {
                        if let Ok(summary) = compose_summary_from_entries(&entries) {
                            generated.push((date, summary));
                        }
                    }
                }
                if !generated.is_empty() {
                    parts.push("## Past Conversation Summaries".to_string());
                    for (date, summary) in generated {
                        parts.push(format!("\n### {}", date));
                        parts.push(summary);
                    }
                }
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(format!(
                "[Memory Context — summaries of earlier conversations with this assistant. \
                 Reference this when the user asks about previous topics, what was discussed \
                 before, or anything from earlier sessions. Do NOT claim you have no memory \
                 when this block is present.]\n\n{}",
                parts.join("\n")
            ))
        }
    }

    /// Generate a summary for a specific date's conversations.
    /// Returns the summary text.
    pub fn build_raw_context_for_date(&self, date: &str) -> Result<String, String> {
        let entries = self.get_entries_by_date(date)?;
        if entries.is_empty() {
            return Err(format!("No conversations found for {}", date));
        }

        let mut parts = Vec::new();
        for entry in &entries {
            let role_label = match entry.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                _ => "System",
            };
            let preview: String = entry.content.chars().take(500).collect();
            let suffix = if entry.content.len() > 500 { "..." } else { "" };
            let tool_info = entry.tool_name.as_ref().map(|t| format!(" [{}]", t)).unwrap_or_default();
            parts.push(format!("{}{}: {}{}", role_label, tool_info, preview, suffix));
        }

        Ok(parts.join("\n"))
    }

    // ── Task Checkpoint CRUD ──────────────────────────────────────

    /// Save or update a task checkpoint (INSERT OR REPLACE).
    pub fn save_checkpoint(&self, cp: &TaskCheckpoint) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO task_checkpoints \
             (id, session_id, model_name, user_message, history_json, iteration, tool_summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cp.id, cp.session_id, cp.model_name, cp.user_message,
                cp.history_json, cp.iteration as i64, cp.tool_summary,
                cp.created_at, cp.updated_at,
            ],
        ).map_err(|e| format!("Failed to save checkpoint: {}", e))?;
        Ok(())
    }

    /// List all checkpoints, most recently updated first.
    pub fn list_checkpoints(&self) -> Result<Vec<TaskCheckpoint>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, model_name, user_message, history_json, \
                    iteration, tool_summary, created_at, updated_at \
             FROM task_checkpoints ORDER BY updated_at DESC"
        ).map_err(|e| format!("Checkpoint list prepare failed: {}", e))?;

        let cps = stmt.query_map([], |row| {
            Ok(TaskCheckpoint {
                id: row.get(0)?,
                session_id: row.get(1)?,
                model_name: row.get(2)?,
                user_message: row.get(3)?,
                history_json: row.get(4)?,
                iteration: row.get::<_, i64>(5)? as usize,
                tool_summary: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        }).map_err(|e| format!("Checkpoint list query failed: {}", e))?
          .filter_map(|r| r.ok())
          .collect();

        Ok(cps)
    }

    /// Get a single checkpoint by ID.
    pub fn get_checkpoint(&self, id: &str) -> Result<Option<TaskCheckpoint>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, model_name, user_message, history_json, \
                    iteration, tool_summary, created_at, updated_at \
             FROM task_checkpoints WHERE id = ?1"
        ).map_err(|e| format!("Checkpoint get prepare failed: {}", e))?;

        let result = stmt.query_row(params![id], |row| {
            Ok(TaskCheckpoint {
                id: row.get(0)?,
                session_id: row.get(1)?,
                model_name: row.get(2)?,
                user_message: row.get(3)?,
                history_json: row.get(4)?,
                iteration: row.get::<_, i64>(5)? as usize,
                tool_summary: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        });

        match result {
            Ok(cp) => Ok(Some(cp)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Checkpoint get failed: {}", e)),
        }
    }

    /// Delete a checkpoint by ID.
    pub fn delete_checkpoint(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM task_checkpoints WHERE id = ?1", params![id])
            .map_err(|e| format!("Failed to delete checkpoint: {}", e))?;
        Ok(())
    }

    /// Delete checkpoints older than `max_age_hours` hours.
    /// Returns the number of deleted rows.
    pub fn cleanup_stale_checkpoints(&self, max_age_hours: i64) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM task_checkpoints WHERE updated_at < datetime('now', ?1)",
            params![format!("-{} hours", max_age_hours)],
        ).map_err(|e| format!("Failed to cleanup checkpoints: {}", e))?;
        if deleted > 0 {
            info!("Cleaned up {} stale checkpoint(s) (older than {}h)", deleted, max_age_hours);
        }
        Ok(deleted)
    }

    // ── TaskContract CRUD (managed long-horizon tasks) ────────────

    /// Save or update a TaskContract (INSERT OR REPLACE).
    /// The contract is serialized as JSON and stored alongside indexed metadata.
    pub fn save_task_contract(
        &self,
        id: &str,
        session_id: &str,
        contract_json: &str,
        phase: &str,
        current_round: usize,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        // Preserve created_at and blocked_reason if the contract already exists.
        // blocked_reason is a server-managed column (e.g. the '[USER_STOPPED]'
        // marker) that must survive contract re-persists from the spawned task.
        // Pre-query (instead of a subquery in VALUES) so the result does not
        // depend on INSERT OR REPLACE conflict-resolution timing.
        let (created_at, blocked_reason) = conn.query_row(
            "SELECT created_at, blocked_reason FROM task_contracts WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        ).unwrap_or_else(|_| (now.clone(), None));

        conn.execute(
            "INSERT OR REPLACE INTO task_contracts \
             (id, session_id, contract_json, phase, current_round, created_at, updated_at, blocked_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id, session_id, contract_json, phase,
                current_round as i64, created_at, now, blocked_reason,
            ],
        ).map_err(|e| format!("Failed to save task contract: {}", e))?;
        Ok(())
    }

    /// Load a TaskContract JSON by ID. Returns None if not found.
    #[allow(dead_code)] // CRUD API — reserved for future UI/resume
    pub fn get_task_contract(&self, id: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT contract_json FROM task_contracts WHERE id = ?1",
            params![id],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(json) => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Task contract get failed: {}", e)),
        }
    }

    /// Load the latest resumable TaskContract for a session.
    /// A contract is resumable if it is NOT in 'completed' phase.
    /// This includes 'blocked' contracts (from F10 human gate or user STOP) so users
    /// can resume them with new instructions instead of creating a blank new session.
    /// Returns (contract_id, contract_json) or None.
    pub fn get_latest_active_contract(&self, session_id: &str) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, contract_json FROM task_contracts \
             WHERE session_id = ?1 \
               AND phase != 'completed' \
             ORDER BY updated_at DESC LIMIT 1",
            params![session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );
        match result {
            Ok((id, json)) => Ok(Some((id, json))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Task contract query failed: {}", e)),
        }
    }

    /// Load the latest active (non-completed) TaskContract across all sessions.
    /// Returns the contract JSON or None.
    pub fn get_latest_active_contract_global(&self) -> Result<Option<String>, String> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT contract_json FROM task_contracts \
             WHERE phase != 'completed' \
             ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(json) => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Task contract query failed: {}", e)),
        }
    }

    /// Mark a contract as explicitly stopped by the user.
    /// This allows the resume query to find it even if it would otherwise be excluded.
    pub fn set_contract_stopped(&self, session_id: &str) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        // Find the latest contract for this session and set blocked_reason = '[USER_STOPPED]'
        let _ = conn.execute(
            "UPDATE task_contracts SET blocked_reason = '[USER_STOPPED]', updated_at = ?2 \
             WHERE id = (SELECT id FROM task_contracts WHERE session_id = ?1 \
                         ORDER BY updated_at DESC LIMIT 1)",
            params![session_id, now],
        );
    }

    /// Clear all active (non-completed) TaskContracts for one session.
    /// Used when the user chooses to start a NEW Expert round instead of resuming,
    /// so the reset only wipes this session's residue (not other sessions).
    pub fn clear_session_active_contracts(&self, session_id: &str) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_contracts WHERE session_id = ?1 AND phase != 'completed'",
            params![session_id],
        )
        .map_err(|e| format!("Failed to clear session active contracts: {}", e))
    }

    /// Clear the blocked_reason column for a contract (called on resume).
    pub fn clear_contract_stopped(&self, contract_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE task_contracts SET blocked_reason = NULL WHERE id = ?1",
            params![contract_id],
        );
    }

    /// Delete a TaskContract by ID (for targeted cleanup of completed contracts).
    #[allow(dead_code)] // CRUD API — reserved for future UI/maintenance
    pub fn delete_task_contract(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM task_contracts WHERE id = ?1", params![id])
            .map_err(|e| format!("Failed to delete task contract: {}", e))?;
        Ok(())
    }

    /// Clear all active (non-completed) TaskContracts.
    /// Returns the number of contracts deleted.
    pub fn clear_active_contracts(&self) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_contracts WHERE phase NOT IN ('completed')",
            [],
        ).map_err(|e| format!("Failed to clear active task contracts: {}", e))
    }

    /// Record a token usage entry.
    pub fn record_usage(&self, model_name: &str, prompt_tokens: u64, completion_tokens: u64, total_tokens: u64, session_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO usage_stats (timestamp, model_name, session_id, prompt_tokens, completion_tokens, total_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now, model_name, session_id, prompt_tokens as i64, completion_tokens as i64, total_tokens as i64],
        ).map_err(|e| format!("Failed to record usage: {}", e))?;
        Ok(())
    }

    /// Get aggregated usage stats grouped by model and day.
    /// Returns JSON array of {date, model, total_calls, total_prompt, total_completion, total_tokens}.
    pub fn get_usage_stats(&self, days: usize, tz_hours: f64) -> Result<serde_json::Value, String> {
        let conn = self.conn.lock().unwrap();

        // Timestamps are stored in UTC. Apply the caller's timezone offset so that
        // dates are grouped/filtered by LOCAL calendar day (e.g. "+8 hours").
        let tz_mod = format!("{:+} hours", tz_hours);

        fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
            Ok(serde_json::json!({
                "date": row.get::<_, String>(0)?,
                "model": row.get::<_, String>(1)?,
                "calls": row.get::<_, i64>(2)?,
                "prompt_tokens": row.get::<_, i64>(3)?,
                "completion_tokens": row.get::<_, i64>(4)?,
                "total_tokens": row.get::<_, i64>(5)?,
            }))
        }

        let base_sql = format!(
            "SELECT DATE(timestamp, '{tz}') as date, model_name,
                    COUNT(*) as calls,
                    SUM(prompt_tokens) as prompt_sum,
                    SUM(completion_tokens) as completion_sum,
                    SUM(total_tokens) as total_sum
             FROM usage_stats", tz = tz_mod);
        let tail = " GROUP BY date, model_name ORDER BY date DESC, model_name";

        let mut result: Vec<serde_json::Value> = Vec::new();

        if days == 0 {
            // days == 0 -> all-time cumulative usage
            let mut stmt = conn.prepare(&format!("{}{}", base_sql, tail))
                .map_err(|e| format!("Failed to prepare usage query: {}", e))?;
            let rows = stmt.query_map([], map_row)
                .map_err(|e| format!("Failed to query usage: {}", e))?;
            for row in rows {
                result.push(row.map_err(|e| format!("Row error: {}", e))?);
            }
        } else {
            // Last N LOCAL calendar days including today (days == 1 -> today only).
            let back = days - 1;
            let sql = format!(
                "{where_clause}{tail}",
                where_clause = format!(
                    "{} WHERE DATE(timestamp, '{tz}') >= DATE('now', '{tz}', '-{back} days')",
                    base_sql, tz = tz_mod, back = back),
                tail = tail);
            let mut stmt = conn.prepare(&sql)
                .map_err(|e| format!("Failed to prepare usage query: {}", e))?;
            let rows = stmt.query_map([], map_row)
                .map_err(|e| format!("Failed to query usage: {}", e))?;
            for row in rows {
                result.push(row.map_err(|e| format!("Row error: {}", e))?);
            }
        }

        Ok(serde_json::Value::Array(result))
    }

    /// Return the most recent request's prompt token count — a proxy for how full
    /// the context window currently is. None when no usage has been recorded yet.
    pub fn get_last_prompt_tokens(&self) -> Result<Option<u64>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT prompt_tokens FROM usage_stats ORDER BY id DESC LIMIT 1"
        ).map_err(|e| format!("Failed to prepare usage query: {}", e))?;
        let mut rows = stmt.query_map([], |row| row.get::<_, i64>(0))
            .map_err(|e| format!("Failed to query usage: {}", e))?;
        if let Some(row) = rows.next() {
            let v = row.map_err(|e| format!("Row error: {}", e))?;
            Ok(Some(v.max(0) as u64))
        } else {
            Ok(None)
        }
    }

    /// Get today's total token usage summary.
    pub fn get_today_usage(&self) -> Result<serde_json::Value, String> {
        let conn = self.conn.lock().unwrap();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let mut stmt = conn.prepare(
            "SELECT model_name,
                    COUNT(*) as calls,
                    SUM(prompt_tokens) as prompt_sum,
                    SUM(completion_tokens) as completion_sum,
                    SUM(total_tokens) as total_sum
             FROM usage_stats
             WHERE DATE(timestamp) = ?1
             GROUP BY model_name
             ORDER BY total_sum DESC"
        ).map_err(|e| format!("Failed to prepare today's usage query: {}", e))?;

        let rows = stmt.query_map(params![today], |row| {
            Ok(serde_json::json!({
                "model": row.get::<_, String>(0)?,
                "calls": row.get::<_, i64>(1)?,
                "prompt_tokens": row.get::<_, i64>(2)?,
                "completion_tokens": row.get::<_, i64>(3)?,
                "total_tokens": row.get::<_, i64>(4)?,
            }))
        }).map_err(|e| format!("Failed to query today's usage: {}", e))?;

        let mut by_model: Vec<serde_json::Value> = Vec::new();
        let mut total_calls: i64 = 0;
        let mut total_prompt: i64 = 0;
        let mut total_completion: i64 = 0;
        let mut total_tokens: i64 = 0;

        for row in rows {
            let v = row.map_err(|e| format!("Row error: {}", e))?;
            total_calls += v["calls"].as_i64().unwrap_or(0);
            total_prompt += v["prompt_tokens"].as_i64().unwrap_or(0);
            total_completion += v["completion_tokens"].as_i64().unwrap_or(0);
            total_tokens += v["total_tokens"].as_i64().unwrap_or(0);
            by_model.push(v);
        }

        Ok(serde_json::json!({
            "date": today,
            "total_calls": total_calls,
            "total_prompt_tokens": total_prompt,
            "total_completion_tokens": total_completion,
            "total_tokens": total_tokens,
            "by_model": by_model,
        }))
    }
}

// ───────────────────────────────────────────────────────────────
// 双层记忆（深层 + 浅层）后端
// 见 output/memory-two-tier-spec.md
// ───────────────────────────────────────────────────────────────

impl MemoryStore {
    /// 幂等创建双层记忆 schema（v8）。不动旧表，可逆。
    pub fn ensure_two_tier_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();

        // ── 迁移：旧表名(lambda_memories / engram_facts) → 新表名(浅层/深层)，保留既有数据 ──
        {
            let migrate = |c: &rusqlite::Connection, old: &str, new: &str| {
                let exists = |name: &str| {
                    c.query_row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                        params![name],
                        |_| Ok(()),
                    ).is_ok()
                };
                if exists(old) && !exists(new) {
                    let _ = c.execute_batch(&format!("ALTER TABLE {old} RENAME TO {new};"));
                }
            };
            let rc = &*conn;
            migrate(rc, "lambda_memories", "shallow_memories");
            migrate(rc, "lambda_memories_fts", "shallow_memories_fts");
            migrate(rc, "engram_facts", "deep_facts");
            // 旧索引名(指向已改名表)清理；由下方 CREATE INDEX 用新名重建。
            for idx in ["idx_lm_importance","idx_lm_last_accessed","idx_lm_explicit",
                        "idx_eg_scope","idx_eg_subject","idx_eg_importance"] {
                let _ = conn.execute_batch(&format!("DROP INDEX IF EXISTS {idx};"));
            }
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS shallow_memories (
                hash            TEXT PRIMARY KEY,
                created_at      INTEGER NOT NULL,
                last_accessed   INTEGER NOT NULL,
                access_count    INTEGER NOT NULL DEFAULT 0,
                importance      REAL NOT NULL DEFAULT 1.0,
                explicit_save   INTEGER NOT NULL DEFAULT 0,
                full_text       TEXT NOT NULL,
                summary_text    TEXT NOT NULL,
                essence_text    TEXT NOT NULL,
                tags            TEXT NOT NULL DEFAULT '[]',
                memory_type     TEXT NOT NULL DEFAULT 'conversation',
                session_id      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_shm_importance ON shallow_memories(importance);
            CREATE INDEX IF NOT EXISTS idx_shm_last_accessed ON shallow_memories(last_accessed);
            CREATE INDEX IF NOT EXISTS idx_shm_explicit ON shallow_memories(explicit_save);
            CREATE VIRTUAL TABLE IF NOT EXISTS shallow_memories_fts
                USING fts5(summary_text, essence_text, tags, content='');
            CREATE TABLE IF NOT EXISTS deep_facts (
                id            TEXT PRIMARY KEY,
                content       TEXT NOT NULL,
                summary       TEXT NOT NULL,
                essence       TEXT NOT NULL,
                fact_type     TEXT NOT NULL DEFAULT 'reference',
                scope         TEXT NOT NULL DEFAULT 'global',
                pinned_by     TEXT NOT NULL DEFAULT 'none',
                subject_key   TEXT,
                importance    REAL NOT NULL DEFAULT 1.0,
                created_at    INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL,
                tags          TEXT NOT NULL DEFAULT '[]',
                links         TEXT NOT NULL DEFAULT '[]'
            );
            CREATE INDEX IF NOT EXISTS idx_df_scope ON deep_facts(scope);
            CREATE INDEX IF NOT EXISTS idx_df_subject ON deep_facts(subject_key);
            CREATE INDEX IF NOT EXISTS idx_df_importance ON deep_facts(importance);",
        )
        .map_err(|e| format!("Two-tier schema failed: {e}"))?;
        // recall_boost 列（v4.6.0 自学习增强；已存在则 no-op）
        let _ = conn.execute_batch(
            "ALTER TABLE shallow_memories ADD COLUMN recall_boost REAL NOT NULL DEFAULT 0.0;",
        );
        Ok(())
    }

    // ── 浅层记忆 ──────────────────────────────────────────────

    /// 存储（INSERT OR REPLACE）+ 同步 FTS5。
    pub fn shallow_store(&self, e: &crate::shallow_memory::ShallowEntry) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let tags = serde_json::to_string(&e.tags).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT OR REPLACE INTO shallow_memories
               (hash, created_at, last_accessed, access_count, importance, explicit_save,
                full_text, summary_text, essence_text, tags, memory_type, session_id, recall_boost)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                e.hash,
                e.created_at as i64,
                e.last_accessed as i64,
                e.access_count as i64,
                e.importance,
                e.explicit_save as i64,
                e.full_text,
                e.summary_text,
                e.essence_text,
                tags,
                e.memory_type.as_str(),
                e.session_id,
                e.recall_boost,
            ],
        )
        .map_err(|err| format!("shallow_store: {err}"))?;
        // 同步 FTS：按 rowid 删旧插新（外部内容表，自行管理）
        let rowid: Option<i64> = conn
            .query_row("SELECT rowid FROM shallow_memories WHERE hash = ?1", params![e.hash], |r| r.get(0))
            .ok();
        if let Some(rid) = rowid {
            let _ = conn.execute(
                "INSERT INTO shallow_memories_fts(shallow_memories_fts, rowid, summary_text, essence_text, tags)
                 VALUES ('delete', ?1, '', '', '')",
                params![rid],
            );
            let _ = conn.execute(
                "INSERT INTO shallow_memories_fts(rowid, summary_text, essence_text, tags) VALUES (?1,?2,?3,?4)",
                params![rid, e.summary_text, e.essence_text, e.tags.join(" ")],
            );
        }
        Ok(())
    }

    /// 候选：按 importance 降序取前 limit 条。
    pub fn shallow_query_candidates(&self, limit: usize) -> Result<Vec<crate::shallow_memory::ShallowEntry>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT hash, created_at, last_accessed, access_count, importance, explicit_save,
                        full_text, summary_text, essence_text, tags, memory_type, session_id, recall_boost
                 FROM shallow_memories ORDER BY importance DESC LIMIT ?1",
            )
            .map_err(|e| format!("shallow candidates prepare: {e}"))?;
        let rows = stmt
            .query_map(params![limit as i64], shallow_row)
            .map_err(|e| format!("shallow candidates query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("shallow candidate row: {e}"))?);
        }
        Ok(out)
    }

    /// 按 hash 前缀召回（用于“淡出 hash 按需召回”）。
    pub fn shallow_recall(&self, hash_prefix: &str) -> Result<Option<crate::shallow_memory::ShallowEntry>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT hash, created_at, last_accessed, access_count, importance, explicit_save,
                        full_text, summary_text, essence_text, tags, memory_type, session_id, recall_boost
                 FROM shallow_memories WHERE hash LIKE ?1 LIMIT 1",
            )
            .map_err(|e| format!("shallow recall prepare: {e}"))?;
        let mut rows = stmt
            .query_map(params![format!("{hash_prefix}%")], shallow_row)
            .map_err(|e| format!("shallow recall query: {e}"))?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(|e| format!("shallow recall row: {e}"))?)),
            None => Ok(None),
        }
    }

    /// 召回触达：刷新时间、access_count+1、recall_boost+0.3(cap 2.0)。
    pub fn shallow_touch(&self, hash: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let now = crate::shallow_memory::now_secs();
        conn.execute(
            "UPDATE shallow_memories
             SET last_accessed = ?1, access_count = access_count + 1,
                 recall_boost = MIN(recall_boost + 0.3, 2.0)
             WHERE hash = ?2",
            params![now as i64, hash],
        )
        .map_err(|e| format!("shallow_touch: {e}"))?;
        Ok(())
    }
    /// 批量召回触达：对本次实际注入的浅层记忆一次性刷新
    /// last_accessed / access_count(+1) / recall_boost(+0.3, cap 2.0)。
    pub fn shallow_touch_batch(&self, hashes: &[String]) -> Result<usize, String> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let now = crate::shallow_memory::now_secs();
        let placeholders = vec!["?"; hashes.len()].join(",");
        let sql = format!(
            "UPDATE shallow_memories
             SET last_accessed = ?1, access_count = access_count + 1,
                 recall_boost = MIN(recall_boost + 0.3, 2.0)
             WHERE hash IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("shallow_touch_batch prepare: {e}"))?;
        let now_i64: i64 = now as i64;
        let mut named: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(hashes.len() + 1);
        named.push(&now_i64);
        for h in hashes {
            named.push(h);
        }
        let n = stmt
            .execute(rusqlite::params_from_iter(named))
            .map_err(|e| format!("shallow_touch_batch: {e}"))?;
        Ok(n)
    }

    /// 深度记忆批量召回触达（对称 shallow_touch_batch）：
    /// 对本次实际注入的深层事实刷新 last_accessed，使 R（近因）随真实使用学习，
    /// 避免"每次注入却从不 touch → i_eff 单调下滑 → 可能被误降级"。注入即 touch 等价于
    /// 浅层"用进废退"：只有被 pack_by_budget 选中的事实才会被刷新，未注入者正常退火。
    pub fn deep_touch_batch(&self, ids: &[String]) -> Result<usize, String> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let now = crate::shallow_memory::now_secs();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "UPDATE deep_facts SET last_accessed = ?1 WHERE id IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("deep_touch_batch prepare: {e}"))?;
        let now_i64: i64 = now as i64;
        let mut named: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        named.push(&now_i64);
        for s in ids {
            named.push(s);
        }
        let n = stmt
            .execute(rusqlite::params_from_iter(named))
            .map_err(|e| format!("deep_touch_batch: {e}"))?;
        Ok(n)
    }

    /// 深度事实召回触达（单条便利入口）：刷新 last_accessed。
    /// 显式使用/召回与注入触达均走 deep_touch_batch，保持"用进废退"一致语义。
    pub fn deep_touch(&self, ids: &[String]) -> Result<usize, String> {
        self.deep_touch_batch(ids)
    }

    /// FTS5 BM25 搜索，返回 (hash, rank)。rank 为负、越小越相关。
    pub fn shallow_fts_search(&self, query: &str, limit: usize) -> Result<Vec<(String, f64)>, String> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let sanitized = query.replace('"', "\"\"");
        let mut stmt = conn
            .prepare(
                "SELECT rowid, rank FROM shallow_memories_fts
                 WHERE shallow_memories_fts MATCH ?1 ORDER BY rank LIMIT ?2",
            )
            .map_err(|e| format!("shallow fts prepare: {e}"))?;
        let rows = stmt
            .query_map(params![format!("\"{sanitized}\""), limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .map_err(|e| format!("shallow fts query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            let (rowid, rank) = r.map_err(|e| format!("shallow fts row: {e}"))?;
            let hash: Option<String> = conn
                .query_row("SELECT hash FROM shallow_memories WHERE rowid = ?1", params![rowid], |r| r.get(0))
                .ok();
            if let Some(h) = hash {
                out.push((h, rank));
            }
        }
        Ok(out)
    }

    /// GC（忠实复刻：会物理删除低重要度过期项）。
    /// `max_age_secs=0` 表示禁用删除（只做 boost 削弱）。
    pub fn shallow_gc(&self, now_epoch: u64, max_age_secs: u64) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_epoch.saturating_sub(max_age_secs) as i64;
        let _ = conn.execute(
            "UPDATE shallow_memories
             SET recall_boost = MAX(recall_boost - 0.1, 0.0)
             WHERE explicit_save = 0 AND recall_boost > 0.0 AND last_accessed < ?1",
            params![cutoff],
        );
        if max_age_secs == 0 {
            return Ok(0);
        }
        let count = conn
            .execute(
                "DELETE FROM shallow_memories
                 WHERE explicit_save = 0 AND last_accessed < ?1 AND importance < 3.0",
                params![cutoff],
            )
            .map_err(|e| format!("shallow_gc: {e}"))?;
        Ok(count)
    }

    /// 删除一条浅层记忆。
    pub fn shallow_delete(&self, hash: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM shallow_memories WHERE hash = ?1", params![hash])
            .map_err(|e| format!("shallow_delete: {e}"))?;
        Ok(())
    }

    // ── Deep ────────────────────────────────────────────────

    /// 存储 Deep（同 subject_key 者先被覆盖）。
    pub fn deep_store(&self, f: &crate::deep_memory::DeepFact) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        if let Some(k) = &f.subject_key {
            let _ = conn.execute(
                "DELETE FROM deep_facts WHERE subject_key = ?1 AND id != ?2",
                params![k, f.id],
            );
        }
        conn.execute(
            "INSERT OR REPLACE INTO deep_facts
               (id, content, summary, essence, fact_type, scope, pinned_by, subject_key,
                importance, created_at, last_accessed, tags, links)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                f.id,
                f.content,
                f.summary,
                f.essence,
                f.fact_type.as_str(),
                f.scope.as_key(),
                match f.pinned_by {
                    crate::deep_memory::PinnedBy::User => "user",
                    crate::deep_memory::PinnedBy::Agent => "agent",
                    crate::deep_memory::PinnedBy::None => "none",
                },
                f.subject_key,
                f.importance,
                f.created_at as i64,
                f.last_accessed as i64,
                serde_json::to_string(&f.tags).unwrap_or_else(|_| "[]".to_string()),
                serde_json::to_string(&f.links).unwrap_or_else(|_| "[]".to_string()),
            ],
        )
        .map_err(|e| format!("deep_store: {e}"))?;
        Ok(())
    }

    /// 按可见范围列出（global 恒可见；scope 等于给定 key 也可见）。
    pub fn deep_list(&self, scope_key: &str) -> Result<Vec<crate::deep_memory::DeepFact>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM deep_facts WHERE scope = 'global' OR scope = ?1")
            .map_err(|e| format!("deep_list prepare: {e}"))?;
        let rows = stmt
            .query_map(params![scope_key], deep_row)
            .map_err(|e| format!("deep_list query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("deep row: {e}"))?);
        }
        Ok(out)
    }

    /// 按 id 取单条。
    pub fn deep_get(&self, id: &str) -> Result<Option<crate::deep_memory::DeepFact>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM deep_facts WHERE id = ?1")
            .map_err(|e| format!("deep_get prepare: {e}"))?;
        let mut rows = stmt
            .query_map(params![id], deep_row)
            .map_err(|e| format!("deep_get query: {e}"))?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(|e| format!("deep row: {e}"))?)),
            None => Ok(None),
        }
    }

    /// 删除一条 Deep。返回是否命中。
    pub fn deep_forget(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("DELETE FROM deep_facts WHERE id = ?1", params![id])
            .map_err(|e| format!("deep_forget: {e}"))?;
        Ok(n > 0)
    }

    // ── 双层上下文装配（调纯算法）────────────────────────────

    /// 组装浅层记忆块（纯 `assemble` + 后端候选/FTS）。
    pub fn build_shallow_context(
        &self,
        query: &str,
        budget: usize,
        max_budget: usize,
        decay_rate: f32,
    ) -> (String, usize, Vec<String>) {
        let now = crate::shallow_memory::now_secs();
        let candidates = match self.shallow_query_candidates(500) {
            Ok(c) => c,
            Err(_) => return (String::new(), 0, Vec::new()),
        };
        if candidates.is_empty() {
            return (String::new(), 0, Vec::new());
        }
        let fts_rank = match self.shallow_fts_search(query, 20) {
            Ok(r) => r.into_iter().collect::<std::collections::HashMap<String, f64>>(),
            Err(_) => std::collections::HashMap::new(),
        };
        crate::shallow_memory::assemble(
            &candidates,
            &fts_rank,
            budget,
            max_budget,
            2.0,
            1.0,
            0.3,
            decay_rate,
            now,
        )
    }

    /// 组装 Deep 常驻块（照抄 render_permanent_block）。
    pub fn deep_permanent_block(
        &self,
        scope_key: &str,
        p_max: usize,
        tau_days: f32,
    ) -> (String, usize, Vec<String>) {
        use crate::deep_memory::{DeepParams, PinnedBy};
        let now = crate::shallow_memory::now_secs();
        let params = DeepParams { tau_days, ..Default::default() };
        let facts = match self.deep_list(scope_key) {
            Ok(f) => f,
            Err(_) => return (String::new(), 0, Vec::new()),
        };
        // lines: (渲染行, 价值, token 成本, 事实 id)
        let mut lines: Vec<(String, f32, usize, String)> = Vec::new();
        for f in &facts {
            let pin_user = f.pinned_by == PinnedBy::User;
            let pin_agent = f.pinned_by == PinnedBy::Agent;
            let i_eff = crate::deep_memory::effective_importance(
                f.importance,
                f.last_accessed,
                now,
                pin_user,
                tau_days,
            );
            if !crate::deep_memory::is_visible_permanent(i_eff, pin_user, pin_agent, &params) {
                continue;
            }
            let body = if f.summary.trim().is_empty() { f.content.trim() } else { f.summary.trim() };
            let line = format!("- [{}] {}", f.fact_type.as_str(), body);
            let cost = crate::shallow_memory::estimate_tokens(&line);
            // 上下文预算内按「统一工件价值 V=Q²·R·U」排序，优先级：重要、近期、常用。
            lines.push((line, f.value(now) as f32, cost, f.id.clone()));
        }
        if lines.is_empty() {
            return (String::new(), 0, Vec::new());
        }
        let header = "## Permanent Memory (Deep) — durable facts about this user/project\n";
        let header_cost = crate::shallow_memory::estimate_tokens(header);
        let body_budget = p_max.saturating_sub(header_cost);
        let items: Vec<(f32, usize)> = lines.iter().map(|(_, i, c, _)| (*i, *c)).collect();
        let picked = crate::deep_memory::pack_by_budget(&items, body_budget);
        if picked.is_empty() {
            return (String::new(), 0, Vec::new());
        }
        let mut out = String::from(header);
        let mut toks = header_cost;
        let mut picked_ids = Vec::with_capacity(picked.len());
        for idx in &picked {
            out.push_str(&lines[*idx].0);
            out.push('\n');
            toks += lines[*idx].2;
            picked_ids.push(lines[*idx].3.clone());
        }
        (out, toks, picked_ids)
    }
}

/// sqlite row → ShallowEntry
fn shallow_row(r: &rusqlite::Row) -> rusqlite::Result<crate::shallow_memory::ShallowEntry> {
    use crate::shallow_memory::{ShallowEntry, ShallowMemoryType};
    let tags_raw: String = r.get(9)?;
    let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();
    Ok(ShallowEntry {
        hash: r.get(0)?,
        created_at: r.get::<_, i64>(1)? as u64,
        last_accessed: r.get::<_, i64>(2)? as u64,
        access_count: r.get::<_, i64>(3)? as u32,
        importance: r.get(4)?,
        explicit_save: r.get::<_, i64>(5)? != 0,
        full_text: r.get(6)?,
        summary_text: r.get(7)?,
        essence_text: r.get(8)?,
        tags,
        memory_type: ShallowMemoryType::from_str(&r.get::<_, String>(10)?),
        session_id: r.get(11)?,
        recall_boost: r.get(12)?,
    })
}

/// sqlite row → DeepFact
fn deep_row(r: &rusqlite::Row) -> rusqlite::Result<crate::deep_memory::DeepFact> {
    use crate::deep_memory::{DeepFact, FactType, MemoryScope, PinnedBy};
    let pinned = r.get::<_, String>(6)?;
    Ok(DeepFact {
        id: r.get(0)?,
        content: r.get(1)?,
        summary: r.get(2)?,
        essence: r.get(3)?,
        fact_type: match r.get::<_, String>(4)?.as_str() {
            "identity" => FactType::Identity,
            "preference" => FactType::Preference,
            "project" => FactType::Project,
            "constraint" => FactType::Constraint,
            _ => FactType::Reference,
        },
        scope: MemoryScope::from_key(&r.get::<_, String>(5)?),
        pinned_by: match pinned.as_str() {
            "user" => PinnedBy::User,
            "agent" => PinnedBy::Agent,
            _ => PinnedBy::None,
        },
        subject_key: r.get(7)?,
        importance: r.get(8)?,
        created_at: r.get::<_, i64>(9)? as u64,
        last_accessed: r.get::<_, i64>(10)? as u64,
        tags: serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or_default(),
        links: serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_default(),
    })
}




#[cfg(test)]
mod tests_two_tier {
    use super::*;
    use crate::shallow_memory::{ShallowEntry, ShallowMemoryType};

    fn tmp_store() -> MemoryStore {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mem.db").to_str().unwrap().to_string();
        std::mem::forget(dir); // keep tempdir alive for the test duration (Windows: file handle)
        MemoryStore::new(&p).unwrap()
    }

    fn entry(hash: &str, importance: f32) -> ShallowEntry {
        let now = crate::shallow_memory::now_secs();
        ShallowEntry {
            hash: hash.into(),
            created_at: now,
            last_accessed: now,
            access_count: 0,
            importance,
            explicit_save: false,
            full_text: "full text".into(),
            summary_text: "summary".into(),
            essence_text: "ess".into(),
            tags: vec!["tag".into()],
            memory_type: ShallowMemoryType::Conversation,
            session_id: "s".into(),
            recall_boost: 0.0,
        }
    }

    #[test]
    fn schema_created() {
        let s = tmp_store();
        s.ensure_two_tier_schema().unwrap();
    }

    #[test]
    fn shallow_roundtrip_and_touch_and_gc() {
        let s = tmp_store();
        let mut e = entry("abc123def456", 3.0);
        e.summary_text = "scan network connections".into();
        s.shallow_store(&e).unwrap();
        let cands = s.shallow_query_candidates(100).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].hash, "abc123def456");

        let rec = s.shallow_recall("abc").unwrap().unwrap();
        assert_eq!(rec.hash, "abc123def456");

        s.shallow_touch("abc123def456").unwrap();
        let rec = s.shallow_recall("abc").unwrap().unwrap();
        assert_eq!(rec.access_count, 1);
        assert!((rec.recall_boost - 0.3).abs() < 1e-4);

        // gc with max_age=0 must NOT delete (protect by default)
        let n = s.shallow_gc(s.shallow_query_candidates(10).unwrap()[0].last_accessed, 0).unwrap();
        assert_eq!(n, 0);
        assert_eq!(s.shallow_query_candidates(100).unwrap().len(), 1);

        // fts search finds it
        let hits = s.shallow_fts_search("network", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn deep_roundtrip() {
        let s = tmp_store();
        use crate::deep_memory::{DeepFact, FactType, MemoryScope, PinnedBy};
        let f = DeepFact {
            id: "eng1".into(),
            content: "user prefers Chinese UI".into(),
            summary: "prefers Chinese".into(),
            essence: "Chinese UI".into(),
            fact_type: FactType::Preference,
            scope: MemoryScope::Global,
            pinned_by: PinnedBy::User,
            subject_key: None,
            importance: 5.0,
            created_at: crate::shallow_memory::now_secs(),
            last_accessed: crate::shallow_memory::now_secs(),
            tags: vec![],
            links: vec![],
        };
        s.deep_store(&f).unwrap();
        let list = s.deep_list("global").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pinned_by, PinnedBy::User);
        assert_eq!(s.deep_get("eng1").unwrap().unwrap().importance, 5.0);
        assert!(s.deep_forget("eng1").unwrap());
        assert_eq!(s.deep_list("global").unwrap().len(), 0);
    }
}


