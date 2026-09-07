//! 深层记忆工具（deep_memory）— 自然语言持久永久记忆。
//!
//! Actions:
//! - `remember` — store a durable fact (upsert by subject_key; user-stated facts pin to User).
//! - `update`   — re-judge / rescore an existing fact (EMA update).
//! - `forget`   — delete the best-matching fact for a query/id.
//! - `recall`   — search permanent facts visible in scope (FTS + list fallback).
//! - `list`     — list visible permanent facts (importance DESC).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::Tool;
use crate::context::ToolContext;
use crate::deep_memory::{self, DeepFact, FactType, MemoryScope, PinnedBy};
use crate::error::AgentResult;
use crate::memory::MemoryStore;

/// Tool for managing the Deep permanent-memory tier (SQLite `deep_facts`).
pub struct DeepMemoryTool {
    memory_store: Arc<MemoryStore>,
}

impl DeepMemoryTool {
    pub fn new(memory_store: Arc<MemoryStore>) -> Self {
        Self { memory_store }
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Scope: only Global is exposed to the main session (no user/chat ids routed yet).
    fn parse_scope(scope: &str) -> MemoryScope {
        let s = scope.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("global") {
            MemoryScope::Global
        } else {
            // Keep arbitrary labels as chat-scoped to avoid cross-session leakage.
            MemoryScope::Chat(s.to_string())
        }
    }

    fn parse_type(t: &str) -> FactType {
        match t.to_ascii_lowercase().as_str() {
            "identity" => FactType::Identity,
            "preference" | "pref" => FactType::Preference,
            "project" => FactType::Project,
            "constraint" => FactType::Constraint,
            _ => FactType::Reference,
        }
    }

    fn parse_tags(tags: &Value) -> Vec<String> {
        tags.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stable id: scope + subject_key (supersede) or scope + content.
    fn make_id(scope: &MemoryScope, subject_key: Option<&str>, content: &str) -> String {
        let basis = match subject_key {
            Some(s) => format!("{}|subj|{s}", scope.as_key()),
            None => format!("{}|body|{content}", scope.as_key()),
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        basis.hash(&mut h);
        format!("eg{:016x}", h.finish())
    }
}

#[async_trait]
impl Tool for DeepMemoryTool {
    fn name(&self) -> &str { "deep_memory" }

    fn description(&self) -> &str {
        "Manage permanent long-term memory (Deep). Actions:\n\
         - 'remember': Store a durable fact. Params: content, summary?, essence?, type?\n\
             (identity|preference|project|constraint|reference), scope? (global|\"chat\"),\n\
             importance? (0-5, default: user=5, agent=4), subject_key?, tags?[].\n\
             User-stated facts pin to User (never auto-demoted).\n\
         - 'update': Re-judge an existing fact. Params: id, importance? (new judgment),\n\
             content?/summary?/essence? to supersede.\n\
         - 'forget': Delete a fact. Params: id (or query to find best match).\n\
         - 'recall': Search permanent facts. Params: query, limit? (default 10).\n\
         - 'list': List visible permanent facts. Params: limit? (default 20).\n\
         Use this when the user says 'remember' / 'forget that' / 'save this' or when you\n\
         learn a durable fact (preference, project convention, constraint, identity)."
    }

    fn is_builtin(&self) -> bool { true }
    fn is_read_only(&self) -> bool { false }
    fn category(&self) -> &str { "write" }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["remember", "update", "forget", "recall", "list"]
                },
                "content": { "type": "string", "description": "Full text of the fact (remember)" },
                "summary": { "type": "string", "description": "One-line summary" },
                "essence": { "type": "string", "description": "5-word-max essence" },
                "type": { "type": "string", "enum": ["identity","preference","project","constraint","reference"] },
                "scope": { "type": "string", "description": "global (default) or a name to make chat-scoped" },
                "importance": { "type": "number", "minimum": 0, "maximum": 5 },
                "subject_key": { "type": "string", "description": "Supersede key; same key replaces old fact" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "id": { "type": "string", "description": "Fact id (update/forget)" },
                "query": { "type": "string", "description": "Recall query" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        let action = args["action"].as_str().ok_or_else(|| "Missing 'action'".to_string())?;
        let now = Self::now();
        match action {
            "remember" => {
                let content = args["content"].as_str().map(str::trim).filter(|s| !s.is_empty())
                    .ok_or_else(|| "Missing 'content' for remember".to_string())?;
                let scope = Self::parse_scope(args["scope"].as_str().unwrap_or("global"));
                let fact_type = Self::parse_type(args["type"].as_str().unwrap_or("reference"));
                let subject_key = args["subject_key"].as_str().map(String::from);
                // Main-session writes default to User pin (sacrosanct), unless explicitly agent.
                let pinned = match args["pinned"].as_str() {
                    Some("agent") => PinnedBy::Agent,
                    _ => PinnedBy::User,
                };
                let importance = args["importance"].as_f64()
                    .map(|x| x as f32)
                    .unwrap_or(if pinned == PinnedBy::User { 5.0 } else { 4.0 })
                    .clamp(deep_memory::IMPORTANCE_MIN, deep_memory::IMPORTANCE_MAX);
                let tags = Self::parse_tags(&args["tags"]);
                let summary = args["summary"].as_str().unwrap_or("").to_string();
                let essence = args["essence"].as_str().unwrap_or("").to_string();
                let id = Self::make_id(&scope, subject_key.as_deref(), content);
                let fact = DeepFact {
                    id: id.clone(),
                    content: content.to_string(),
                    summary,
                    essence,
                    fact_type,
                    scope,
                    pinned_by: pinned,
                    subject_key,
                    importance,
                    created_at: now,
                    last_accessed: now,
                    tags,
                    links: vec![],
                };
                self.memory_store.deep_store(&fact)?;
                Ok(json!({
                    "success": true,
                    "action": "remember",
                    "id": id,
                    "pinned": match pinned { PinnedBy::User => "user", PinnedBy::Agent => "agent", PinnedBy::None => "none" },
                    "importance": importance,
                    "message": "Deep fact stored"
                }))
            }
            "update" => {
                let id = args["id"].as_str().ok_or_else(|| "Missing 'id' for update".to_string())?;
                let existing = self.memory_store.deep_get(id)?
                    .ok_or_else(|| format!("No deep fact with id '{id}'"))?;
                let mut fact = existing.clone();
                if let Some(i) = args["importance"].as_f64() {
                    fact.importance = deep_memory::ema_update(fact.importance, i as f32, 0.4);
                }
                if let Some(c) = args["content"].as_str() { if !c.is_empty() { fact.content = c.to_string(); } }
                if let Some(s) = args["summary"].as_str() { if !s.is_empty() { fact.summary = s.to_string(); } }
                if let Some(e) = args["essence"].as_str() { if !e.is_empty() { fact.essence = e.to_string(); } }
                fact.last_accessed = now;
                self.memory_store.deep_store(&fact)?;
                Ok(json!({
                    "success": true,
                    "action": "update",
                    "id": id,
                    "importance": fact.importance,
                    "message": "Deep fact updated"
                }))
            }
            "forget" => {
                let id = args["id"].as_str().map(String::from);
                if let Some(id) = id {
                    let hit = self.memory_store.deep_forget(&id)?;
                    return Ok(json!({ "success": true, "action": "forget", "deleted": hit, "id": id }));
                }
                // Fallback: match by query across visible facts.
                let query = args["query"].as_str().unwrap_or("").to_lowercase();
                let facts = self.memory_store.deep_list("global")?;
                let best = facts.iter().find(|f| {
                    let q = query.clone();
                    f.content.to_lowercase().contains(&q) || f.summary.to_lowercase().contains(&q) || f.essence.to_lowercase().contains(&q)
                }).cloned();
                match best {
                    Some(f) => {
                        let hit = self.memory_store.deep_forget(&f.id)?;
                        Ok(json!({ "success": true, "action": "forget", "deleted": hit, "id": f.id, "content": f.content }))
                    }
                    None => Ok(json!({ "success": true, "action": "forget", "deleted": false, "message": "No matching fact" })),
                }
            }
            "recall" => {
                let query = args["query"].as_str().unwrap_or("").to_string();
                let limit = args["limit"].as_u64().unwrap_or(10) as usize;
                // Full visible list (no FTS hook in this store yet), filter + sort by importance.
                let mut facts = self.memory_store.deep_list("global")?;
                if !query.trim().is_empty() {
                    let q = query.to_lowercase();
                    facts.retain(|f| {
                        f.content.to_lowercase().contains(&q)
                            || f.summary.to_lowercase().contains(&q)
                            || f.essence.to_lowercase().contains(&q)
                            || f.tags.iter().any(|t| t.to_lowercase().contains(&q))
                    });
                }
                facts.sort_by(|a, b| b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal));
                // 召回触达（A2）：对本次显式召回的事实刷新 last_accessed（reheat）。
                let touched_ids: Vec<String> = facts.iter().take(limit).map(|f| f.id.clone()).collect();
                if !touched_ids.is_empty() {
                    let _ = self.memory_store.deep_touch(&touched_ids);
                }
                let out: Vec<Value> = facts.into_iter().take(limit).map(|f| {
                    json!({
                        "id": f.id,
                        "content": f.content,
                        "summary": f.summary,
                        "essence": f.essence,
                        "fact_type": f.fact_type.as_str(),
                        "importance": f.importance,
                        "pinned": match f.pinned_by { PinnedBy::User => "user", PinnedBy::Agent => "agent", _ => "none" }
                    })
                }).collect();
                Ok(json!({ "success": true, "action": "recall", "count": out.len(), "results": out }))
            }
            "list" => {
                let limit = args["limit"].as_u64().unwrap_or(20) as usize;
                let mut facts = self.memory_store.deep_list("global")?;
                facts.sort_by(|a, b| b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal));
                let out: Vec<Value> = facts.into_iter().take(limit).map(|f| {
                    json!({
                        "id": f.id,
                        "content": f.content,
                        "summary": f.summary,
                        "essence": f.essence,
                        "fact_type": f.fact_type.as_str(),
                        "importance": f.importance,
                        "pinned": match f.pinned_by { PinnedBy::User => "user", PinnedBy::Agent => "agent", _ => "none" }
                    })
                }).collect();
                Ok(json!({ "success": true, "action": "list", "count": out.len(), "results": out }))
            }
            _ => Err(format!("Unknown action '{action}'. Valid: remember, update, forget, recall, list").into()),
        }
    }
}

