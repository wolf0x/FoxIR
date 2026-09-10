//! Evidence ledger tool — cross-round, incident-scoped structured state for
//! "已收集证据 / 有效凭证 / 已确认结论 / 已排除项".
//!
//! Design intent (see SDD): evidence is ORDERED structured state that the agent
//! records as it works and re-reads each round, so it does not re-run tools or
//! re-derive already-confirmed conclusions. Stored as JSON in
//! workspace/evidence.json, keyed by incident_id (NOT by todos.json, which has
//! a "clear on completion" lifecycle that would wipe evidence exactly when the
//! report stage needs it).
//!
//! Fields per entry:
//!   - id          : content hash (dedup key; re-adding same content updates it)
//!   - kind        : file | ioc | finding | credential | conclusion | excluded
//!   - status      : collected (raw) | verified (trusted) | excluded (ruled out)
//!   - confidence  : 0.0..1.0
//!   - sensitive   : true => NEVER included in the per-round general injection
//!                   (credentials/private data only reachable via explicit list)
//!   - source      : which tool/command produced it
//!   - created_at  : unix epoch seconds
//!
//! Actions:
//!   - add    : register/replace an evidence entry (dedup by content hash)
//!   - update : set status (collected|verified|excluded) and/or confidence
//!   - list   : read entries for an incident (optionally include sensitive)
//!   - remove : drop one entry by id
//!   - clear  : drop all entries for an incident
//!
//! A shared free function `build_evidence_block` renders the per-round injected
//! block with a budget cap and EXCLUDES sensitive entries by default.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Tool;
use crate::context::ToolContext;
use crate::error::AgentResult;

/// Evidence status semantics:
///   collected — raw result just captured, NOT yet trusted (lead only)
///   verified  — corroborated / usable as strong evidence (report-grade)
///   excluded  — ruled out / superseded (do NOT re-investigate)
pub const STATUS_COLLECTED: &str = "collected";
pub const STATUS_VERIFIED: &str = "verified";
pub const STATUS_EXCLUDED: &str = "excluded";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub confidence: f64,
    pub sensitive: bool,
    pub content: String,
    pub source: String,
    pub created_at: u64,
}

impl EvidenceEntry {
    /// Stable identity derived from content + kind (dedup key, e.g. "cmd").
    fn make_id(kind: &str, content: &str) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        kind.hash(&mut hasher);
        content.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EvidenceFile {
    /// incident_id -> entries (BTreeMap keeps JSON output deterministic).
    incidents: BTreeMap<String, Vec<EvidenceEntry>>,
}

/// Tool for tracking an incident-scoped evidence ledger.
pub struct EvidenceTool {
    workspace_dir: String,
}

impl EvidenceTool {
    pub fn new(workspace_dir: String) -> Self {
        Self { workspace_dir }
    }

    fn file_path(&self) -> PathBuf {
        PathBuf::from(&self.workspace_dir).join("evidence.json")
    }

    fn load(&self) -> EvidenceFile {
        let path = self.file_path();
        if !path.exists() {
            return EvidenceFile::default();
        }
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &EvidenceFile) -> Result<(), String> {
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| format!("Serialize error: {}", e))?;
        std::fs::write(self.file_path(), json)
            .map_err(|e| format!("Write error: {}", e))?;
        Ok(())
    }

    fn sanitize_kind(kind: &str) -> String {
        let k = kind.trim().to_lowercase();
        if k.is_empty() {
            "finding".to_string()
        } else if matches!(k.as_str(), "file" | "ioc" | "finding" | "credential" | "conclusion" | "excluded") {
            k
        } else {
            "finding".to_string()
        }
    }

    fn sanitize_status(status: &str) -> String {
        match status.trim().to_lowercase().as_str() {
            "verified" => STATUS_VERIFIED.to_string(),
            "excluded" => STATUS_EXCLUDED.to_string(),
            _ => STATUS_COLLECTED.to_string(),
        }
    }

    fn entry_to_json(e: &EvidenceEntry) -> Value {
        json!({
            "id": e.id,
            "kind": e.kind,
            "status": e.status,
            "confidence": e.confidence,
            "sensitive": e.sensitive,
            "content": e.content,
            "source": e.source,
            "created_at": e.created_at,
        })
    }
}

#[async_trait]
impl Tool for EvidenceTool {
    fn name(&self) -> &str { "evidence" }

    fn description(&self) -> &str {
        "Track an incident-scoped evidence ledger: already-collected evidence files, \
         valid credentials, confirmed conclusions, and items ruled out. Prevents \
         re-running tools and re-deriving confirmed results across rounds. Use when \
         a multi-step investigation/ops task must remember what it already found. \
         Actions:\n\
         - 'add': register/replace an entry. Args: incident_id, content, kind \
           (file|ioc|finding|credential|conclusion|excluded), status \
           (collected|verified|excluded), confidence (0..1), sensitive (bool), source.\n\
         - 'update': change status/confidence of an entry. Args: incident_id, id, status, confidence.\n\
         - 'list': read entries for an incident. Args: incident_id, include_sensitive (bool). \
           Sensitive entries are hidden from the per-round injection; use this to view them explicitly.\n\
         - 'remove': drop one entry. Args: incident_id, id.\n\
         - 'clear': drop all entries for an incident. Args: incident_id.\n\
         After adding/updating evidence, call action='list' to confirm the new ledger state."
    }

    fn is_builtin(&self) -> bool { true }
    fn is_read_only(&self) -> bool { false }
    fn category(&self) -> &str { "write" }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "update", "list", "remove", "clear"] },
                "incident_id": { "type": "string", "description": "Incident/task identity for scoping. Defaults to session_id." },
                "id": { "type": "string", "description": "Entry id (returned by add/list). Required for update/remove." },
                "content": { "type": "string", "description": "Evidence content: file path, IOC, fact, credential, conclusion, or ruled-out item." },
                "kind": { "type": "string", "enum": ["file", "ioc", "finding", "credential", "conclusion", "excluded"] },
                "status": { "type": "string", "enum": ["collected", "verified", "excluded"] },
                "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                "sensitive": { "type": "boolean", "description": "true excludes this from the per-round general injection (use for credentials/private data)." },
                "source": { "type": "string", "description": "Which tool/command produced this evidence." },
                "include_sensitive": { "type": "boolean", "description": "For 'list' only: include sensitive entries." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> AgentResult<Value> {
        let action = args["action"].as_str().unwrap_or("");
        let default_incident = ctx.base.base.session_id.clone();
        let incident = args["incident_id"].as_str().map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| if default_incident.is_empty() { "default".to_string() } else { default_incident });

        let mut file = self.load();

        match action {
            "add" => {
                let content = args["content"].as_str().unwrap_or("").trim().to_string();
                if content.is_empty() {
                    return Err("Missing non-empty 'content' for add action".into());
                }
                let kind = Self::sanitize_kind(args["kind"].as_str().unwrap_or("finding"));
                let status = Self::sanitize_status(args["status"].as_str().unwrap_or(STATUS_COLLECTED));
                let confidence = args["confidence"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
                let sensitive = args["sensitive"].as_bool().unwrap_or(false);
                let source = args["source"].as_str().unwrap_or("").to_string();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let id = EvidenceEntry::make_id(&kind, &content);
                let entry = EvidenceEntry {
                    id: id.clone(),
                    kind,
                    status,
                    confidence,
                    sensitive,
                    content,
                    source,
                    created_at: now,
                };

                let entries = file.incidents.entry(incident.clone()).or_insert_with(Vec::new);
                if let Some(existing) = entries.iter_mut().find(|e| e.id == id) {
                    existing.status = entry.status;
                    existing.confidence = entry.confidence;
                    existing.sensitive = entry.sensitive;
                    existing.source = entry.source.clone();
                } else if entry.status == STATUS_VERIFIED {
                    entries.insert(0, entry);
                } else {
                    entries.push(entry);
                }
                self.save(&file)?;
                Ok(json!({ "success": true, "action": "add", "incident_id": incident, "id": id }))
            }

            "update" => {
                let id = args["id"].as_str().ok_or_else(|| "Missing 'id' for update action".to_string())?;
                let entries = file.incidents.get_mut(&incident)
                    .ok_or_else(|| format!("No entries for incident '{}'", incident))?;
                let e = entries.iter_mut().find(|e| e.id == id)
                    .ok_or_else(|| format!("Entry '{}' not found", id))?;
                if let Some(s) = args["status"].as_str() {
                    e.status = Self::sanitize_status(s);
                }
                if let Some(c) = args["confidence"].as_f64() {
                    e.confidence = c.clamp(0.0, 1.0);
                }
                let out_status = e.status.clone();
                let out_confidence = e.confidence;
                self.save(&file)?;
                Ok(json!({ "success": true, "action": "update", "incident_id": incident, "id": id, "status": out_status, "confidence": out_confidence }))
            }

            "list" => {
                let include_sensitive = args["include_sensitive"].as_bool().unwrap_or(false);
                let entries = file.incidents.get(&incident).cloned().unwrap_or_default();
                let filtered: Vec<Value> = entries.iter()
                    .filter(|e| include_sensitive || !e.sensitive)
                    .map(Self::entry_to_json)
                    .collect();
                Ok(json!({
                    "success": true, "action": "list", "incident_id": incident,
                    "count": filtered.len(), "total": entries.len(),
                    "items": filtered,
                }))
            }

            "remove" => {
                let id = args["id"].as_str().ok_or_else(|| "Missing 'id' for remove action".to_string())?;
                let removed = if let Some(entries) = file.incidents.get_mut(&incident) {
                    let before = entries.len();
                    entries.retain(|e| e.id != id);
                    before != entries.len()
                } else { false };
                if removed {
                    self.save(&file)?;
                    Ok(json!({ "success": true, "action": "remove", "incident_id": incident, "id": id }))
                } else {
                    Err(format!("Entry '{}' not found in incident '{}'", id, incident).into())
                }
            }

            "clear" => {
                file.incidents.remove(&incident);
                self.save(&file)?;
                Ok(json!({ "success": true, "action": "clear", "incident_id": incident }))
            }

            _ => Err(format!("Unknown evidence action '{}'", action).into()),
        }
    }
}

/// Render the per-round injected evidence block for an incident.
///
/// - Only non-sensitive entries are included (credentials/private stay out of
///   the general prompt; reachable only via an explicit `evidence list`).
/// - Ordered: verified first (confidence desc), then collected (confidence desc),
///   then excluded.
/// - Budget-capped via `max_chars` so the ledger cannot grow the context without
///   bound (finite-brain protection). A pointer to `evidence list` is appended.
/// - Returns `None` when the incident has nothing non-sensitive to show.
pub fn build_evidence_block(workspace_dir: &str, incident_id: &str, max_chars: usize) -> Option<String> {
    if workspace_dir.is_empty() || incident_id.is_empty() {
        return None;
    }
    let path = PathBuf::from(workspace_dir).join("evidence.json");
    if !path.exists() {
        return None;
    }
    let file: EvidenceFile = std::fs::read_to_string(&path).ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    let entries = file.incidents.get(incident_id)?;

    let mut visible: Vec<&EvidenceEntry> = entries.iter()
        .filter(|e| !e.sensitive)
        .collect();
    if visible.is_empty() {
        return None;
    }
    visible.sort_by(|a, b| {
        let rank = |e: &EvidenceEntry| match e.status.as_str() {
            STATUS_VERIFIED => 0i32,
            STATUS_COLLECTED => 1,
            STATUS_EXCLUDED => 2,
            _ => 3,
        };
        rank(a).cmp(&rank(b))
            .then_with(|| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut lines = String::new();
    lines.push_str("\n## Evidence Ledger (incident) — already collected; reuse, do not re-run:\n");
    let mut used = lines.len();
    let total_visible = visible.len();
    for e in &visible {
        let mark = match e.status.as_str() {
            STATUS_VERIFIED => "[v]".to_string(),
            STATUS_EXCLUDED => "[x]".to_string(),
            _ => format!("[c:{:.1}]", e.confidence),
        };
        let line = format!("{} {} — {}\n", mark, e.content, e.source);
        if used + line.len() > max_chars {
            let truncated = format!("... and {} more (use `evidence list` for full ledger)\n", total_visible);
            lines.push_str(&truncated);
            break;
        }
        used += line.len();
        lines.push_str(&line);
    }
    // Convergence guard: new evidence may override an old "verified" conclusion.
    lines.push_str("(New evidence may supersede an earlier 'verified' conclusion — update, do not silently trust stale entries.)\n");
    Some(lines)
}

/// Convenience wrapper used by callers that only have a session id.
pub fn build_evidence_block_for_session(workspace_dir: &str, session_id: &str) -> Option<String> {
    let incident = if session_id.trim().is_empty() { "default" } else { session_id };
    build_evidence_block(workspace_dir, incident, 1600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{CallbackContext, ReadonlyContext};

    fn tool(ws: &str) -> EvidenceTool {
        EvidenceTool::new(ws.to_string())
    }

    fn ctx(ws: &str, session_id: &str) -> ToolContext {
        let base = ReadonlyContext::new("inv".into(), "agent".into(), session_id.to_string());
        ToolContext::new(CallbackContext::new(base), "fc".into(), ws.to_string(), ws.to_string())
    }

    fn tmp_ws(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("foxir_evidence_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn add_list_roundtrip() {
        let ws = tmp_ws("roundtrip");
        let t = tool(&ws);
        let c = ctx(&ws, "sess-1");
        let r = t.execute(json!({"action":"add","content":"C:\\case\\evtx\\e1.evtx","kind":"file","status":"verified","source":"winrm"}), &c).await.unwrap();
        let id = r["id"].as_str().unwrap().to_string();
        let r2 = t.execute(json!({"action":"add","content":"C:\\case\\evtx\\e1.evtx","kind":"file","status":"verified","source":"winrm"}), &c).await.unwrap();
        assert_eq!(r2["id"].as_str().unwrap(), &id);
        let l = t.execute(json!({"action":"list"}), &c).await.unwrap();
        assert_eq!(l["count"].as_u64().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn block_excludes_sensitive() {
        let ws = tmp_ws("block");
        let t = tool(&ws);
        let inc = "inc-7";
        let c = ctx(&ws, "sess-2");
        let _ = t.execute(json!({"action":"add","incident_id":inc,"content":"password=abc123","kind":"credential","sensitive":true,"source":"env"}), &c).await.unwrap();
        let _ = t.execute(json!({"action":"add","incident_id":inc,"content":"C:\\x\\ioc.txt","kind":"file","status":"verified","source":"ir_file"}), &c).await.unwrap();
        let _ = t.execute(json!({"action":"add","incident_id":inc,"content":"67.42.1.1","kind":"ioc","status":"collected","source":"ir_network"}), &c).await.unwrap();
        assert!(build_evidence_block(&ws, "sess-2", 1600).is_none());
        let block = build_evidence_block(&ws, inc, 1600).unwrap();
        assert!(block.contains("ioc.txt"));
        assert!(block.contains("67.42.1.1"));
        assert!(!block.contains("abc123"), "sensitive leaked: {block}");
        let capped = build_evidence_block(&ws, inc, 30).unwrap_or_default();
        assert!(capped.contains("evidence list"));
        let l = t.execute(json!({"action":"list","incident_id":inc,"include_sensitive":true}), &c).await.unwrap();
        assert_eq!(l["count"].as_u64().unwrap(), 3);
        let l2 = t.execute(json!({"action":"list","incident_id":inc}), &c).await.unwrap();
        assert_eq!(l2["count"].as_u64().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn update_status_and_remove() {
        let ws = tmp_ws("update");
        let t = tool(&ws);
        let inc = "inc-u";
        let c = ctx(&ws, "sess-3");
        let v = t.execute(json!({"action":"add","incident_id":inc,"content":"67.42.1.1","kind":"ioc","status":"collected","source":"ir_network"}), &c).await.unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let _ = t.execute(json!({"action":"update","incident_id":inc,"id":id,"status":"verified","confidence":0.95}), &c).await.unwrap();
        let block = build_evidence_block(&ws, inc, 1600).unwrap();
        assert!(block.contains("[v]"), "verified should be [v]: {block}");
        let _ = t.execute(json!({"action":"remove","incident_id":inc,"id":id}), &c).await.unwrap();
        let l = t.execute(json!({"action":"list","incident_id":inc}), &c).await.unwrap();
        assert_eq!(l["count"].as_u64().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
