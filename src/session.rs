//! Session management inspired by ADK-RUST's SessionService trait.
//!
//! Provides session persistence with InMemory backend.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{AgentError, AgentResult};
use crate::model::ChatMessage;

/// Per-session metadata kept in the lightweight session index (title, timestamps,
/// soft-delete flag). This is the "session registry" layer used by the multi-session
/// navigation UI; it is decoupled from the heavy per-session conversation history
/// which lives in `AppState.sessions` and `memory.db` (keyed by session_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub main: bool,
}

impl SessionMeta {
    pub fn new(title: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            title,
            created_at: now,
            updated_at: now,
            deleted: false,
            main: false,
        }
    }
}

/// A single session representing a conversation.
/// Modeled after ADK-RUST's Session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub app_name: String,
    pub user_id: String,
    pub state: HashMap<String, Value>,
    pub conversation_history: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    pub fn new(id: String, app_name: String, user_id: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            app_name,
            user_id,
            state: HashMap::new(),
            conversation_history: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Append a message to conversation history.
    pub fn append_message(&mut self, message: ChatMessage) {
        self.conversation_history.push(message);
        self.updated_at = Utc::now();
    }

    /// Get conversation history (full or truncated).
    pub fn conversation_history(&self, max_events: Option<usize>) -> &[ChatMessage] {
        match max_events {
            Some(max) if max < self.conversation_history.len() => {
                &self.conversation_history[self.conversation_history.len() - max..]
            }
            _ => &self.conversation_history,
        }
    }

    /// Get a state value.
    pub fn get_state(&self, key: &str) -> Option<&Value> {
        self.state.get(key)
    }

    /// Set a state value.
    pub fn set_state(&mut self, key: String, value: Value) {
        self.state.insert(key, value);
        self.updated_at = Utc::now();
    }
}

/// Session service trait — manages session lifecycle.
/// Modeled after ADK-RUST's SessionService trait.
pub trait SessionService: Send + Sync {
    fn create(&self, app_name: &str, user_id: &str) -> AgentResult<Session>;
    fn get(&self, session_id: &str) -> AgentResult<Session>;
    fn list(&self, app_name: &str, user_id: &str) -> AgentResult<Vec<Session>>;
    fn delete(&self, session_id: &str) -> AgentResult<()>;
    fn append_message(&self, session_id: &str, message: ChatMessage) -> AgentResult<()>;
}

/// In-memory session service — sessions live in RAM, lost on restart.
/// Modeled after ADK-RUST's InMemorySessionService.
pub struct InMemorySessionService {
    sessions: Mutex<HashMap<String, Session>>,
}

impl InMemorySessionService {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySessionService {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionService for InMemorySessionService {
    fn create(&self, app_name: &str, user_id: &str) -> AgentResult<Session> {
        let id = uuid::Uuid::new_v4().to_string();
        let session = Session::new(id, app_name.to_string(), user_id.to_string());
        let mut sessions = self.sessions.lock().map_err(|e| AgentError::session(format!("Lock: {}", e)))?;
        sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    fn get(&self, session_id: &str) -> AgentResult<Session> {
        let sessions = self.sessions.lock().map_err(|e| AgentError::session(format!("Lock: {}", e)))?;
        sessions.get(session_id).cloned().ok_or_else(|| AgentError::not_found(
            crate::error::ErrorComponent::Session,
            format!("Session '{}' not found", session_id),
        ))
    }

    fn list(&self, app_name: &str, user_id: &str) -> AgentResult<Vec<Session>> {
        let sessions = self.sessions.lock().map_err(|e| AgentError::session(format!("Lock: {}", e)))?;
        Ok(sessions.values()
            .filter(|s| s.app_name == app_name && s.user_id == user_id)
            .cloned()
            .collect())
    }

    fn delete(&self, session_id: &str) -> AgentResult<()> {
        let mut sessions = self.sessions.lock().map_err(|e| AgentError::session(format!("Lock: {}", e)))?;
        sessions.remove(session_id);
        Ok(())
    }

    fn append_message(&self, session_id: &str, message: ChatMessage) -> AgentResult<()> {
        let mut sessions = self.sessions.lock().map_err(|e| AgentError::session(format!("Lock: {}", e)))?;
        let session = sessions.get_mut(session_id).ok_or_else(|| AgentError::not_found(
            crate::error::ErrorComponent::Session,
            format!("Session '{}' not found", session_id),
        ))?;
        session.append_message(message);
        Ok(())
    }
}

/// Lightweight per-process session registry backing the multi-session navigation
/// UI. Tracks title/timestamps/soft-delete for each session id and persists to a
/// single JSON file so the list survives process restarts. It deliberately does
/// NOT own conversation history — that remains in `AppState.sessions` (memory)
/// and `memory.db` (SQLite, keyed by session_id), which already give per-session
/// isolation. This layer only surfaces which sessions exist and how to label them.
pub struct SessionIndex {
    path: std::path::PathBuf,
    metas: Mutex<HashMap<String, SessionMeta>>,
}

impl SessionIndex {
    /// Load the index from `path` (creating an empty one if missing/corrupt).
    pub fn load(path: std::path::PathBuf) -> Self {
        let mut metas = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, SessionMeta>>(&raw).ok())
            .unwrap_or_default();
        Self::ensure_main(&mut metas);
        Self { path, metas: Mutex::new(metas) }
    }

    /// Persist current index to disk (best-effort; failures are logged by caller).
    pub fn save(&self) -> Result<(), String> {
        let metas = self.metas.lock().map_err(|e| format!("SessionIndex lock: {e}"))?;
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let raw = serde_json::to_string_pretty(&*metas).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, raw).map_err(|e| e.to_string())
    }

    /// Register the session if unknown, bump `updated_at`, and optionally set a
    /// title on first sight. Cheap and safe to call on every active-session adopt.
    pub fn touch(&self, session_id: &str, title: Option<String>) -> Result<(), String> {
        let mut metas = self.metas.lock().map_err(|e| format!("SessionIndex lock: {e}"))?;
        let now = Utc::now();
        match metas.get_mut(session_id) {
            Some(m) => {
                m.updated_at = now;
                if title.is_some() && m.title.is_none() {
                    m.title = title;
                }
            }
            None => {
                let mut meta = SessionMeta::new(title);
                meta.updated_at = now;
                metas.insert(session_id.to_string(), meta);
                // A main Chat session always exists; designate this one if none yet.
                Self::ensure_main(&mut metas);
            }
        }
        drop(metas);
        self.save()
    }

    /// Rename a session (returns Err if absent).
    pub fn rename(&self, session_id: &str, title: &str) -> Result<(), String> {
        let mut metas = self.metas.lock().map_err(|e| format!("SessionIndex lock: {e}"))?;
        let meta = metas.get_mut(session_id).ok_or_else(|| format!("Session not found: {session_id}"))?;
        meta.title = Some(title.trim().to_string());
        meta.updated_at = Utc::now();
        drop(metas);
        self.save()
    }

    /// Soft-delete a session so it disappears from the list (history retained in
    /// memory/SQLite for potential recovery).
    pub fn soft_delete(&self, session_id: &str) -> Result<(), String> {
        let mut metas = self.metas.lock().map_err(|e| format!("SessionIndex lock: {e}"))?;
        if let Some(meta) = metas.get_mut(session_id) {
            if meta.main {
                return Err("The main Chat session cannot be deleted".to_string());
            }
            meta.deleted = true;
            meta.updated_at = Utc::now();
        }
        drop(metas);
        self.save()
    }

    /// All non-deleted sessions, newest-first by `updated_at`.
    pub fn list(&self) -> Vec<(String, SessionMeta)> {
        let metas = self.metas.lock().map(|g| g.clone()).unwrap_or_default();
        let mut v: Vec<(String, SessionMeta)> = metas
            .into_iter()
            .filter(|(_, m)| !m.deleted)
            .collect();
        v.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
        v
    }

    /// Guarantee exactly one main Chat session exists: if none is marked yet,
    /// promote the earliest-created non-deleted session. The main Chat session
    /// is a persistent primary conversation and is never deleted.
    fn ensure_main(metas: &mut HashMap<String, SessionMeta>) {
        if metas.values().any(|m| m.main) {
            return;
        }
        if let Some((_, meta)) = metas
            .iter_mut()
            .filter(|(_, m)| !m.deleted)
            .min_by_key(|(_, m)| m.created_at)
        {
            meta.main = true;
        }
    }

    /// The id of the always-present main Chat session (None only before any
    /// session has been created).
    pub fn main(&self) -> Option<String> {
        self.metas.lock().ok()
            .and_then(|g| g.iter().find(|(_, m)| m.main).map(|(id, _)| id.clone()))
    }

    pub fn get(&self, session_id: &str) -> Option<SessionMeta> {
        self.metas.lock().ok().and_then(|g| g.get(session_id).cloned())
    }
}

#[test]
fn session_index_touch_rename_delete_roundtrip() {
    let dir = std::env::temp_dir().join(format!("foxir_sess_test_{}", uuid::Uuid::new_v4()));
    let path = dir.join("session_index.json");
    let idx = SessionIndex::load(path.clone());
    idx.touch("sess-1", Some("First".into())).unwrap();
    idx.touch("sess-2", None).unwrap();
    assert_eq!(idx.list().len(), 2);
    idx.rename("sess-1", "Renamed").unwrap();
    assert_eq!(idx.list()[0].1.title.as_deref(), Some("Renamed"));
    idx.soft_delete("sess-2").unwrap();
    assert_eq!(idx.list().len(), 1);

    // Persistence survives a reload.
    let idx2 = SessionIndex::load(path.clone());
    assert_eq!(idx2.list().len(), 1);
    assert_eq!(idx2.list()[0].1.title.as_deref(), Some("Renamed"));
    let _ = std::fs::remove_dir_all(&dir);
}
