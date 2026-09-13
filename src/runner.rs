//! Runner — separates orchestration from the agent loop.
//! Modeled after ADK-RUST's Runner which handles session management,
//! plugin hooks, and agent dispatch.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::HashMap;
use tokio::sync::Mutex;
use tracing::info;

use crate::agent::{Agent, EventStream};
use crate::checkpoint::ATaskCheckpointer;
use crate::context::{InvocationContext, ReadonlyContext};
use crate::error::{AgentError, AgentResult};
use crate::log::ConversationLogger;
use crate::model::ChatMessage;
use crate::permission::PendingMap;
use crate::managed::permission_profile::PermissionProfile;
use crate::session::{InMemorySessionService, SessionService};

/// State restored from a checkpoint for resuming an interrupted task.
pub struct ResumeState {
    pub history: Vec<ChatMessage>,
    pub start_iteration: usize,
}

/// The Runner is the outer orchestration runtime.
/// It manages sessions, builds context, dispatches to the agent, and persists events.
/// Modeled after ADK-RUST's Runner.
pub struct Runner {
    agent: Arc<dyn Agent>,
    session_service: Arc<dyn SessionService>,
    logger: Arc<ConversationLogger>,
    app_name: String,
    checkpointer: Option<ATaskCheckpointer>,
    trim_redundant_tool_calls: Arc<AtomicBool>,
    knowledge_pre_retrieval: Arc<AtomicBool>,
    sop_replay: Arc<AtomicBool>,
    budget_dashboard: Arc<AtomicBool>,
    budget_sink: Option<std::sync::Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>>,
    enable_context_scaling: Arc<AtomicBool>,
    max_inline_chars: Arc<AtomicUsize>,
    skill_listing_strategy: Arc<AtomicUsize>,
    skill_max_inline_chars: Arc<AtomicUsize>,
    skill_catalog_max: Arc<AtomicUsize>,
    skill_hot_top_k: Arc<AtomicUsize>,
    /// Whether the root run may spawn sub-agent workers (SDD v1.5 2.x).
    can_spawn: bool,
    /// Operational mode for this run (Instant unlocks orchestration at depth 0).
    mode: crate::context::AgentMode,
}

/// Builder for Runner (modeled after ADK-RUST's RunnerConfig builder).
pub struct RunnerBuilder {
    agent: Option<Arc<dyn Agent>>,
    session_service: Option<Arc<dyn SessionService>>,
    logger: Option<Arc<ConversationLogger>>,
    app_name: String,
    checkpointer: Option<ATaskCheckpointer>,
    knowledge_pre_retrieval: Arc<AtomicBool>,
    sop_replay: Arc<AtomicBool>,
    budget_dashboard: Arc<AtomicBool>,
    budget_sink: Option<std::sync::Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>>,
    trim_redundant_tool_calls: Arc<AtomicBool>,
    enable_context_scaling: Arc<AtomicBool>,
    max_inline_chars: Arc<AtomicUsize>,
    skill_listing_strategy: Arc<AtomicUsize>,
    skill_max_inline_chars: Arc<AtomicUsize>,
    skill_catalog_max: Arc<AtomicUsize>,
    skill_hot_top_k: Arc<AtomicUsize>,
    /// Whether the root run may spawn sub-agent workers (SDD v1.5 2.x).
    can_spawn: bool,
    /// Operational mode for this run (Instant unlocks orchestration at depth 0).
    mode: crate::context::AgentMode,
}

impl RunnerBuilder {
    pub fn new() -> Self {
        Self {
            agent: None,
            session_service: None,
            logger: None,
            app_name: "RustAgent".to_string(),
            checkpointer: None,
            trim_redundant_tool_calls: Arc::new(AtomicBool::new(true)),
            knowledge_pre_retrieval: Arc::new(AtomicBool::new(true)),
            sop_replay: Arc::new(AtomicBool::new(true)),
            budget_dashboard: Arc::new(AtomicBool::new(true)),
            budget_sink: None,
            enable_context_scaling: Arc::new(AtomicBool::new(true)),
            max_inline_chars: Arc::new(AtomicUsize::new(120_000)),
            skill_listing_strategy: Arc::new(AtomicUsize::new(0)),
            skill_max_inline_chars: Arc::new(AtomicUsize::new(6000)),
            skill_catalog_max: Arc::new(AtomicUsize::new(40)),
            skill_hot_top_k: Arc::new(AtomicUsize::new(3)),
            can_spawn: false,
            mode: crate::context::AgentMode::Instant,
        }
    }

    pub fn agent(mut self, agent: Arc<dyn Agent>) -> Self {
        self.agent = Some(agent);
        self
    }

    pub fn session_service(mut self, service: Arc<dyn SessionService>) -> Self {
        self.session_service = Some(service);
        self
    }

    pub fn logger(mut self, logger: Arc<ConversationLogger>) -> Self {
        self.logger = Some(logger);
        self
    }

    pub fn app_name(mut self, name: &str) -> Self {
        self.app_name = name.to_string();
        self
    }

    pub fn checkpointer(mut self, cp: ATaskCheckpointer) -> Self {
        self.checkpointer = Some(cp);
        self
    }

    pub fn trim_redundant_tool_calls(mut self, v: Arc<AtomicBool>) -> Self {
        self.trim_redundant_tool_calls = v;
        self
    }

    pub fn knowledge_pre_retrieval(mut self, v: Arc<AtomicBool>) -> Self {
        self.knowledge_pre_retrieval = v;
        self
    }
    pub fn sop_replay(mut self, v: Arc<AtomicBool>) -> Self {
        self.sop_replay = v;
        self
    }
    pub fn budget_dashboard(mut self, v: Arc<AtomicBool>) -> Self {
        self.budget_dashboard = v;
        self
    }
    pub fn budget_sink(mut self, v: Option<std::sync::Arc<std::sync::Mutex<Option<crate::context_arbiter::BudgetReport>>>>) -> Self {
        self.budget_sink = v;
        self
    }

    pub fn enable_context_scaling(mut self, v: Arc<AtomicBool>) -> Self {
        self.enable_context_scaling = v;
        self
    }

    pub fn max_inline_chars(mut self, v: Arc<AtomicUsize>) -> Self {
        self.max_inline_chars = v;
        self
    }

    pub fn skill_listing_strategy(mut self, v: Arc<AtomicUsize>) -> Self {
        self.skill_listing_strategy = v;
        self
    }

    pub fn skill_max_inline_chars(mut self, v: Arc<AtomicUsize>) -> Self {
        self.skill_max_inline_chars = v;
        self
    }

    pub fn skill_catalog_max(mut self, v: Arc<AtomicUsize>) -> Self {
        self.skill_catalog_max = v;
        self
    }

    pub fn skill_hot_top_k(mut self, v: Arc<AtomicUsize>) -> Self {
        self.skill_hot_top_k = v;
        self
    }

    pub fn with_can_spawn(mut self, enabled: bool) -> Self {
        self.can_spawn = enabled;
        self
    }

    pub fn with_mode(mut self, mode: crate::context::AgentMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn build(self) -> AgentResult<Runner> {
        let agent = self.agent.ok_or_else(|| AgentError::config("Runner requires an agent"))?;
        let session_service = self.session_service
            .unwrap_or_else(|| Arc::new(InMemorySessionService::new()));
        let logger = self.logger
            .ok_or_else(|| AgentError::config("Runner requires a logger"))?;

        Ok(Runner {
            agent,
            session_service,
            logger,
            app_name: self.app_name,
            checkpointer: self.checkpointer,
            trim_redundant_tool_calls: self.trim_redundant_tool_calls,
            knowledge_pre_retrieval: self.knowledge_pre_retrieval,
            sop_replay: self.sop_replay,
            budget_dashboard: self.budget_dashboard,
            budget_sink: self.budget_sink,
            enable_context_scaling: self.enable_context_scaling,
            max_inline_chars: self.max_inline_chars,
            skill_listing_strategy: self.skill_listing_strategy,
            skill_max_inline_chars: self.skill_max_inline_chars,
            skill_catalog_max: self.skill_catalog_max,
            skill_hot_top_k: self.skill_hot_top_k,
            can_spawn: self.can_spawn,
            mode: self.mode,
        })
    }
}

impl Runner {
    pub fn builder() -> RunnerBuilder {
        RunnerBuilder::new()
    }

    /// Create a clone of this Runner with `can_spawn` forcibly disabled.
    /// Used by ManagedRunner to ensure Expert Executor rounds cannot spawn
    /// sub-agents (enforcing the "Expert = pure serial" invariant).
    pub fn without_spawn(&self) -> Runner {
        Runner {
            agent: self.agent.clone(),
            session_service: self.session_service.clone(),
            logger: self.logger.clone(),
            app_name: self.app_name.clone(),
            checkpointer: self.checkpointer.clone(),
            trim_redundant_tool_calls: self.trim_redundant_tool_calls.clone(),
            knowledge_pre_retrieval: self.knowledge_pre_retrieval.clone(),
            sop_replay: self.sop_replay.clone(),
            budget_dashboard: self.budget_dashboard.clone(),
            budget_sink: self.budget_sink.clone(),
            enable_context_scaling: self.enable_context_scaling.clone(),
            max_inline_chars: self.max_inline_chars.clone(),
            skill_listing_strategy: self.skill_listing_strategy.clone(),
            skill_max_inline_chars: self.skill_max_inline_chars.clone(),
            skill_catalog_max: self.skill_catalog_max.clone(),
            skill_hot_top_k: self.skill_hot_top_k.clone(),
            can_spawn: false,
            mode: self.mode,
        }
    }

    /// Run the agent for a given user message and return the event stream.
    /// The runner handles session creation, context building, and event persistence.
    pub async fn run(
        &self,
        user_message: &str,
        session_id: &str,
        model_name: &str,
        max_iterations: usize,
        history: Vec<ChatMessage>,
        permissions: Arc<Mutex<HashMap<String, bool>>>,
        permission_pending: PendingMap,
        preauth_profile: Option<Arc<PermissionProfile>>,
        fallback_model: Option<String>,
        rabbit_hole_threshold: usize,
        context_window: usize,
        context_window_threshold: usize,
        tool_timeout_secs: u64,
        max_tool_retries: usize,
        images: Vec<String>,
        checkpoint_id: Option<String>,
        resume_checkpoint: Option<ResumeState>,
        output_dir: Option<String>,
    ) -> AgentResult<EventStream> {
        info!("Runner dispatching to agent '{}' (session: {})", self.agent.name(), session_id);

        // Build invocation context
        let invocation_id = uuid::Uuid::new_v4().to_string();
        let base_ctx = ReadonlyContext::new(
            invocation_id,
            self.agent.name().to_string(),
            session_id.to_string(),
        );
        let mut ctx = InvocationContext::new(
            base_ctx,
            self.agent.name().to_string(),
            model_name.to_string(),
            max_iterations,
        ).with_history(history)
         .with_permissions(permissions)
         .with_permission_pending(permission_pending)
         .with_preauth_profile(preauth_profile)
         .with_fallback_model(fallback_model)
         .with_rabbit_hole_threshold(rabbit_hole_threshold)
         .with_trim_redundant_tool_calls(self.trim_redundant_tool_calls.load(Ordering::SeqCst))
         .with_knowledge_pre_retrieval(self.knowledge_pre_retrieval.load(Ordering::SeqCst))
         .with_sop_replay(self.sop_replay.load(Ordering::SeqCst))
.with_budget_dashboard(self.budget_dashboard.load(Ordering::SeqCst))
.with_budget_sink(self.budget_sink.clone())
         .with_context_window(context_window)
         .with_context_window_threshold(context_window_threshold)
         .with_enable_context_scaling(self.enable_context_scaling.load(Ordering::SeqCst))
         .with_max_inline_chars(self.max_inline_chars.load(Ordering::SeqCst))
         .with_skill_listing_strategy(crate::skill::SkillListingStrategy::from_index(self.skill_listing_strategy.load(Ordering::SeqCst)))
         .with_skill_max_inline_chars(self.skill_max_inline_chars.load(Ordering::SeqCst))
         .with_skill_catalog_max(self.skill_catalog_max.load(Ordering::SeqCst))
         .with_skill_hot_top_k(self.skill_hot_top_k.load(Ordering::SeqCst))
         .with_tool_timeout_secs(tool_timeout_secs)
         .with_max_tool_retries(max_tool_retries)
         .with_can_spawn(self.can_spawn)
         .with_mode(self.mode)
         .with_tool_output_dir(output_dir);

        // Wire checkpoint/resume state if provided.
        if let Some(resume) = resume_checkpoint {
            ctx = ctx.with_resume_state(resume.history, resume.start_iteration);
        }
        if let Some(cp_id) = checkpoint_id {
            ctx = ctx.with_checkpoint_id(cp_id);
        }
        if let Some(ref cp) = self.checkpointer {
            ctx = ctx.with_checkpointer(cp.clone());
        }

        // Log user message
        self.logger.log_user_message(session_id, user_message);

        // Dispatch to agent
        let event_stream = self.agent.run(&ctx, user_message, images).await?;

        // Wrap the stream to log events
        let logger = self.logger.clone();
        let sid = session_id.to_string();
        let wrapped_stream = async_stream::stream! {
            tokio::pin!(event_stream);
            while let Some(result) = event_stream.next().await {
                match &result {
                    Ok(event) => {
                        logger.log_event(&sid, event);
                    }
                    Err(e) => {
                        tracing::warn!("Event stream error: {}", e);
                    }
                }
                yield result;
            }
        };

        Ok(Box::pin(wrapped_stream))
    }



    pub fn agent(&self) -> &dyn Agent {
        self.agent.as_ref()
    }

    pub fn session_service(&self) -> &dyn SessionService {
        self.session_service.as_ref()
    }
}


use futures::StreamExt;


#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AgentMode;
    use futures::stream;

    /// Fake agent that captures the runtime ctx it receives from Runner::run.
    struct CaptureAgent {
        captured: Arc<std::sync::Mutex<Option<(AgentMode, u8, bool)>>>,
    }
    #[async_trait::async_trait]
    impl Agent for CaptureAgent {
        fn name(&self) -> &str { "capture" }
        fn description(&self) -> &str { "capture ctx" }
        async fn run(
            &self,
            ctx: &InvocationContext,
            _user_message: &str,
            _images: Vec<String>,
        ) -> AgentResult<EventStream> {
            *self.captured.lock().unwrap() = Some((ctx.mode, ctx.depth, ctx.can_spawn));
            Ok(Box::pin(stream::empty()))
        }
    }

    fn perms() -> Arc<Mutex<HashMap<String, bool>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn runner_expert_mode_propagates_to_runtime_ctx() {
        // SDD v1.5 2.x wiring: an Expert orchestration root must reach the agent
        // as ctx.mode == Expert && depth == 0 && can_spawn, so the Orchestrator
        // creation branch (llm_agent.rs) actually triggers instead of staying dead.
        let captured = Arc::new(std::sync::Mutex::new(None));
        let fake = CaptureAgent { captured: captured.clone() };
        let runner = Runner::builder()
            .agent(Arc::new(fake))
            .logger(Arc::new(ConversationLogger::new(std::env::temp_dir().to_str().unwrap())))
            .with_can_spawn(true)
            .with_mode(AgentMode::Expert)
            .build()
            .unwrap();
        let _ = runner.run(
            "hi", "sess", "m", 5, vec![],
            perms(), Arc::new(Mutex::new(HashMap::new())), None, None,
            3, 64000, 80, 60, 3, vec![], None, None, None,
        ).await.unwrap();
        let (mode, depth, can_spawn) = captured.lock().unwrap().take().unwrap();
        assert_eq!(mode, AgentMode::Expert, "Expert mode must reach the agent ctx");
        assert_eq!(depth, 0, "root run depth must be 0");
        assert!(can_spawn, "Expert orchestration root must be allowed to spawn");
    }

    #[tokio::test]
    async fn runner_default_stays_instant_non_spawning() {
        // Default (legacy) runner must remain an Instant, non-spawning agent —
        // zero diff when orchestration is disabled.
        let captured = Arc::new(std::sync::Mutex::new(None));
        let fake = CaptureAgent { captured: captured.clone() };
        let runner = Runner::builder()
            .agent(Arc::new(fake))
            .logger(Arc::new(ConversationLogger::new(std::env::temp_dir().to_str().unwrap())))
            .build()
            .unwrap();
        let _ = runner.run(
            "hi", "sess", "m", 5, vec![],
            perms(), Arc::new(Mutex::new(HashMap::new())), None, None,
            3, 64000, 80, 60, 3, vec![], None, None, None,
        ).await.unwrap();
        let (mode, depth, can_spawn) = captured.lock().unwrap().take().unwrap();
        assert_eq!(mode, AgentMode::Instant);
        assert_eq!(depth, 0);
        assert!(!can_spawn);
    }
}

