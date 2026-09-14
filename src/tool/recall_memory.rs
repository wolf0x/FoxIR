//! On-demand memory recall tool (recall_memory).
//!
//! Replaces the old "auto-inject a deep recall blob every recall-query". The
//! agent calls this read-only tool only when it decides it needs specific past
//! conversation detail. Results are distilled to a bounded top-k block so a
//! reply can be answered directly from memory instead of re-reading archives.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use super::Tool;
use crate::context::ToolContext;
use crate::error::AgentResult;
use crate::memory::MemoryStore;

/// Tool for on-demand recall of past conversations from the local memory store.
pub struct RecallMemoryTool {
    memory_store: Arc<MemoryStore>,
}

impl RecallMemoryTool {
    pub fn new(memory_store: Arc<MemoryStore>) -> Self {
        Self { memory_store }
    }
}

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str { "recall_memory" }

    fn description(&self) -> &str {
        "Search past conversations stored in local memory for exchanges relevant to a query.\n\
         Use this when you need to recall specific details ('what versions were affected?',\n\
         'what did we conclude earlier?') that may not be in the visible context. Returns a\n\
         bounded, relevance-ranked summary of matching past messages and recent daily summaries.\n\
         Answer directly from the returned content; do not re-read source archives to restate it."
    }

    fn is_builtin(&self) -> bool { true }
    fn is_read_only(&self) -> bool { true }
    fn category(&self) -> &str { "read" }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to recall (topic / question)" },
                "days": { "type": "integer", "minimum": 1, "maximum": 90, "description": "Look-back window in days (default 14)" },
                "max_items": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Max matching entries to return (default 8)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> AgentResult<Value> {
        let query = args["query"].as_str().map(str::trim).filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing non-empty 'query' for recall_memory".to_string())?;
        let days = args["days"].as_i64().unwrap_or(14).clamp(1, 90) as usize;
        let max_items = args["max_items"].as_i64().unwrap_or(8).clamp(1, 20) as usize;

        // Distilled, budget-capped block: top-k hits + a slim recent-summary tail.
        let budget_chars = (max_items * 450).min(6000);
        let block = self
            .memory_store
            .build_recall_context(query, days, budget_chars)
            .unwrap_or_else(|| "No relevant past conversations found.".to_string());

        Ok(json!({
            "query": query,
            "days": days,
            "max_items": max_items,
            "recall": block
        }))
    }
}
