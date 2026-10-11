//! Mid-run interjection & follow-up queue.
//!
//! Two independent per-session channels:
//! - **pending follow-ups** (default): messages the user sends while a task is
//!   running. They are NOT injected into the running task. They are held here
//!   and dispatched by the server as the *next* task(s) after the current one
//!   completes (FIFO). The running agent loop never drains this channel.
//! - **insert-now** (explicit button): messages the user chose to inject into
//!   the *current* running task as supplementary context. Only this channel is
//!   drained by the agent loop, so the running LLM sees them as the next user
//!   turns.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

type PendingQueue = Arc<Mutex<HashMap<String, VecDeque<(String, bool)>>>>;
type InsertQueue = Arc<Mutex<HashMap<String, VecDeque<String>>>>;

fn pending() -> &'static PendingQueue {
    static Q: OnceLock<PendingQueue> = OnceLock::new();
    Q.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

fn insert_channel() -> &'static InsertQueue {
    static Q: OnceLock<InsertQueue> = OnceLock::new();
    Q.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Queue a follow-up task to run after the current task finishes (default
/// path when the user does not press "insert"). Synchronous and non-blocking.
///
/// `echo` controls whether the dispatcher should send a `queued_run` event to
/// the client when this item actually starts running:
/// - `true` for interject-origin items (the client never rendered a bubble for
///   them, so `queued_run` is what surfaces the user message once).
/// - `false` for chat-while-running items (the client already rendered its own
///   send bubble, so echoing again would duplicate the user message).
pub fn push_pending(session_id: &str, content: String, echo: bool) {
    pending()
        .lock()
        .unwrap()
        .entry(session_id.to_string())
        .or_default()
        .push_back((content, echo));
}

/// Pop the next queued follow-up for a session (FIFO), returning its content and
/// the `echo` flag. Returns `None` when the queue is empty. The server
/// dispatches these as sequential tasks after the current run completes.
pub fn pop_pending(session_id: &str) -> Option<(String, bool)> {
    let mut q = pending().lock().unwrap();
    let entry = q.get_mut(session_id)?;
    let item = entry.pop_front();
    if entry.is_empty() {
        q.remove(session_id);
    }
    item
}

/// Whether a session still has queued follow-up tasks that nobody has
/// dispatched yet. A run finishing with no client attached cannot dispatch them
/// (the per-connection loop is gone), so the server uses this to make the
/// orphaned queue visible in the log instead of losing it silently.
pub fn has_pending(session_id: &str) -> bool {
    pending()
        .lock()
        .map(|q| q.get(session_id).map(|e| !e.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

/// Inject a message into the CURRENT running task (explicit "insert" button).
/// Picked up by the agent loop on its next iteration.
pub fn push_insert(session_id: &str, content: String) {
    insert_channel()
        .lock()
        .unwrap()
        .entry(session_id.to_string())
        .or_default()
        .push_back(content);
}

/// Drain all insert-now messages for a session (FIFO). Called by the agent
/// loop at the start of each iteration so the running LLM sees them as the
/// next user turns.
pub fn drain_insert(session_id: &str) -> Vec<String> {
    insert_channel()
        .lock()
        .unwrap()
        .remove(session_id)
        .map(|q| q.into_iter().collect())
        .unwrap_or_default()
}


#[cfg(test)]
mod tests {
    use super::*;

    // Queued follow-ups pop in FIFO order and the queue empties.
    #[test]
    fn pending_queue_is_fifo_and_empties() {
        let sid = "test-pending-1";
        for m in ["task-a", "task-b", "task-c"] {
            push_pending(sid, m.to_string(), true);
        }
        assert_eq!(pop_pending(sid).map(|(c, _)| c).as_deref(), Some("task-a"));
        assert_eq!(pop_pending(sid).map(|(c, _)| c).as_deref(), Some("task-b"));
        assert_eq!(pop_pending(sid).map(|(c, _)| c).as_deref(), Some("task-c"));
        assert_eq!(pop_pending(sid), None);
        assert_eq!(pop_pending(sid), None, "empty queue keeps returning None");
    }

    // Insert-now messages drain in FIFO order.
    #[test]
    fn insert_channel_drains_fifo() {
        let sid = "test-insert-1";
        push_insert(sid, "ctx-1".to_string());
        push_insert(sid, "ctx-2".to_string());
        let drained = drain_insert(sid);
        assert_eq!(drained, vec!["ctx-1".to_string(), "ctx-2".to_string()]);
        assert!(drain_insert(sid).is_empty(), "after drain the insert channel is empty");
    }

    // The two channels are isolated: a pending follow-up is NOT visible to the
    // insert drain and vice versa (running loop must never see pending items).
    #[test]
    fn channels_are_isolated() {
        let sid = "test-iso-1";
        push_pending(sid, "follow-up".to_string(), true);
        push_insert(sid, "insert-now".to_string());
        let drained = drain_insert(sid);
        assert_eq!(drained, vec!["insert-now".to_string()]);
        assert!(drain_insert(sid).is_empty());
        // The pending follow-up is untouched by draining inserts.
        assert_eq!(pop_pending(sid).map(|(c, _)| c).as_deref(), Some("follow-up"));
        assert_eq!(pop_pending(sid), None);
    }
}
