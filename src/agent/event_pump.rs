//! Event pump (SDD v1.5 §7.4.2).
//!
//! A single in-Manager task that drains every worker's own event channel with
//! `FuturesUnordered` so workers never block on a full buffer.
//!
//! Since P0-A/P0-B, worker *raw* fragments (text/thinking/tool_call/tool_result/
//! progress) are no longer broadcast into the parent run's WebSocket stream —
//! the structured milestones are delivered independently by the typed
//! `subagent_*`/`budget_update` events (`Orchestrator::emit_typed`). This
//! collapses the wait-phase WS flood that could stall/drop the connection and
//! tear down an in-flight orchestration run. The EventPump therefore only
//! *drains* each worker channel (bounded by `WORKER_CHANNEL_CAP`) and drops the
//! events; `run_worker` still consumes a worker's own stream to build its
//! summary/evidence/tokens, so no worker data is lost.
//!
//! Registration is a *bounded* channel (cap = max_concurrent*2, set by the
//! Orchestrator) so the register hop carries backpressure — see the accepted
//! spec delta (avoids §7.4.2's UnboundedReceiver deadlock).

use std::future::Future;
use std::pin::Pin;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::agent::event::AgentEvent;

/// Per-worker event channel capacity (SDD §7.4.2).
pub const WORKER_CHANNEL_CAP: usize = 128;

type WorkerEvent = (Option<AgentEvent>, tokio::sync::mpsc::Receiver<AgentEvent>);

/// The Manager's single event aggregation task.
pub struct EventPump {
    reg_rx: tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Receiver<AgentEvent>>,
}

impl EventPump {
    /// `reg_rx` receives newly-spawned worker channels (bounded by the
    /// Orchestrator). The pump only drains worker channels; it does not forward
    /// events to the parent stream (see module docs).
    pub fn new(
        reg_rx: tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Receiver<AgentEvent>>,
    ) -> Self {
        Self { reg_rx }
    }

    fn push(
        mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
        pending: &mut FuturesUnordered<Pin<Box<dyn Future<Output = WorkerEvent> + Send>>>,
    ) {
        pending.push(Box::pin(async move {
            let ev = rx.recv().await;
            (ev, rx)
        }));
    }

    /// Dynamically aggregate worker channels until the registration channel
    /// closes and every worker channel has drained, then end.
    pub async fn run(mut self) {
        let mut pending: FuturesUnordered<Pin<Box<dyn Future<Output = WorkerEvent> + Send>>> =
            FuturesUnordered::new();
        let mut reg_done = false;
        loop {
            tokio::select! {
                reg = self.reg_rx.recv(), if !reg_done => {
                    match reg {
                        Some(rx) => Self::push(rx, &mut pending),
                        None => reg_done = true,
                    }
                }
                got = pending.next(), if !pending.is_empty() => {
                    match got {
                        Some((Some(ev), rx)) => {
                            self.forward(ev).await;
                            Self::push(rx, &mut pending);
                        }
                        Some((None, _rx)) => { /* worker channel closed; drop */ }
                        None => {}
                    }
                }
                else => break,
            }
            if reg_done && pending.is_empty() {
                break;
            }
        }
    }

    /// Drain one worker event. The event is intentionally dropped (not sent to
    /// the parent stream) — see the module docs. Returning immediately keeps the
    /// drain loop moving so the worker's bounded channel never blocks.
    async fn forward(&self, e: AgentEvent) {
        let _ = e;
    }
}

/// Push a compact completion summary for a finished sub-agent into its own
/// worker channel so the EventPump forwards it like any other event.
pub fn broadcast_subagent_result(
    worker_tx: &Option<tokio::sync::mpsc::Sender<AgentEvent>>,
    result: &crate::context::SubAgentResult,
) {
    if let Some(tx) = worker_tx {
        let ev = AgentEvent::text(
            &format!(
                "[sub-agent:{}] done (status={:?}) summary: {}",
                result.role, result.status, result.summary
            ),
            &result.run_id,
            &result.role,
        );
        let _ = tx.try_send(ev);
    }
}
