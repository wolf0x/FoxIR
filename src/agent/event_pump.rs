//! Event pump (SDD v1.5 §7.4.2).
//!
//! A single in-Manager task that dynamically aggregates every worker's own
//! event channel with `FuturesUnordered` and forwards events to the parent
//! run's event stream with tiered backpressure:
//!   - `TextDelta`: `try_send` (droppable — the hop most tolerant of loss)
//!   - everything else: `send().await` (guaranteed, single-hop backpressure)
//! Workers each write to their own `mpsc::channel(cap = WORKER_CHANNEL_CAP)`
//! and never block; the EventPump is the only await-backpressure point
//! (§7.4.2). Registration is a *bounded* channel (cap = max_concurrent*2,
//! set by the Orchestrator) so the register hop also has backpressure — see
//! the accepted spec delta (avoids §7.4.2's UnboundedReceiver deadlock).

use std::future::Future;
use std::pin::Pin;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::agent::event::AgentEvent;
use crate::error::AgentResult;

/// Per-worker event channel capacity (SDD §7.4.2).
pub const WORKER_CHANNEL_CAP: usize = 128;

type WorkerEvent = (Option<AgentEvent>, tokio::sync::mpsc::Receiver<AgentEvent>);

/// The Manager's single event aggregation task.
pub struct EventPump {
    reg_rx: tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Receiver<AgentEvent>>,
    ws_tx: tokio::sync::mpsc::Sender<AgentResult<AgentEvent>>,
}

impl EventPump {
    /// `reg_rx` receives newly-spawned worker channels (bounded by the
    /// Orchestrator); `ws_tx` is the parent run's event stream sink.
    pub fn new(
        reg_rx: tokio::sync::mpsc::Receiver<tokio::sync::mpsc::Receiver<AgentEvent>>,
        ws_tx: tokio::sync::mpsc::Sender<AgentResult<AgentEvent>>,
    ) -> Self {
        Self { reg_rx, ws_tx }
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

    /// Tiered backpressure (§7.4.2): `TextDelta` is droppable; all other events
    /// are awaited (one controlled hop). (`PromptAugmented` does not exist in
    /// this codebase's `AgentEvent`, so `_` covers every remaining variant.)
    async fn forward(&self, e: AgentEvent) {
        match &e {
            AgentEvent::TextDelta { .. } => {
                let _ = self.ws_tx.try_send(Ok(e));
            }
            _ => {
                let _ = self.ws_tx.send(Ok(e)).await;
            }
        }
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
