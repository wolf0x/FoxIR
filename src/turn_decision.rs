//! TurnDecision — Agent 轮次决策的单一纯函数核。
//!
//! 对应设计：`output/SDD-Architecture-Risks.md` §5 D3（根治设计）。
//!
//! ## 要解决的问题
//! 旧实现用「散文子串猜测」（正文/思考里出现工具名、"让我查"等意图词）去判断
//! "模型是不是想调工具却忘了"，进而强制重提示（"正在重新组织工具调用"）。这会
//! 把**合法的纯文本回答**（尤其记忆召回：复述的教训里含 `browser_cdp` 等工具名）
//! 误判为"漏发工具调用"，逼出本不需要的工具执行。根因是**分层缺失**：把
//! 「能力 / 传输解析 / 决策 / 完成信号」揉进一个 `if`，用猜测恢复一个本不该丢失的决策。
//!
//! ## 本核的原则（根治）
//! - **决策只由结构化信号推导**：解析出的 tool_calls 数、归一化 finish_reason、
//!   可见正文有无、坏信封标志、能力位、是否跑过工具、是否已重试。**绝不扫描正文子串。**
//! - **"无工具调用"是一等合法终止态**：有可见正文即 `Answer`，信任模型的文本决定。
//! - **层级防火墙**：本核只决定"如何处理这一个 provider turn"，**不是任务完成权威**。
//!   任务完成权威在 Instant=迭代预算(+可选 TODO 门)、Expert=Manager Route+TaskContract。
//! - **长任务续跑保证**：`tool_calls>0 → RunTools` 为最高优先，任何文本/finish_reason
//!   都不打断；被删的旧重提示本就受 `!has_executed_tools` 门控（只在首轮工具执行前生效），
//!   故对已跑起来的大 TODO / 长任务零影响。
//! - 无 I/O、无 LLM、全枚举、可全测（与 `context_arbiter` / `deep_memory` 同血统）。
//!
//! ## Stage A（本文件）
//! 仅立地基：类型 + `decide_turn` 纯核 + 全枚举单测。**尚未接线**到 llm_agent 主循环
//! （Stage B）与 managed/子代理/heartbeat（Stage C）。

/// 归一化后的停止信号（吸收各 provider 的 finish_reason 拼写差异）。
///
/// 由**边界层**在调用 [`decide_turn`] 前填充；边界还负责 microclaw 式归一：
/// provider 声称 `tool_use` 但实际没解析出调用 → 归一为 [`FinishReason::Stop`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// 自然结束（stop / end_turn）。
    #[default]
    Stop,
    /// 模型请求调用工具（tool_calls / tool_use）——边界已保证其确有调用，否则归一为 Stop。
    ToolCalls,
    /// 被 max_tokens 截断（length / max_tokens）。
    Length,
    /// 其它/未知（content_filter、缺失等）。
    Other,
}

impl FinishReason {
    /// 把 provider 原始 finish_reason 字符串归一化（大小写/别名容错）。
    ///
    /// 缺失/空 → `None`；[`decide_turn`] 对 `None` 有防御（有调用→RunTools，有正文→Answer）。
    pub fn normalize(raw: Option<&str>) -> Option<FinishReason> {
        let s = raw?.trim();
        if s.is_empty() {
            return None;
        }
        let f = match s.to_ascii_lowercase().as_str() {
            "stop" | "end_turn" | "endturn" | "complete" | "completed" | "finished" => {
                FinishReason::Stop
            }
            "tool_calls" | "tool_call" | "tool_use" | "function_call" => FinishReason::ToolCalls,
            "length" | "max_tokens" | "token_limit" | "context_length" => FinishReason::Length,
            _ => FinishReason::Other,
        };
        Some(f)
    }
}

/// 决策所需的**结构化**信号（全部来自 provider 归一化边界，不含任何散文判断）。
///
/// 关键约束：[`TurnSignals::malformed_envelope`] 只能由**解析器**在检测到"真实工具信封
/// （```json 围栏 / 工具协议 tag）却解析失败"时置位，**绝不**因正文提到工具名而置位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnSignals {
    /// 已解析出的结构化工具调用数量（native 字段或提示契约解析、含截断修复后）。
    pub tool_calls: usize,
    /// provider 权威停止信号（已在边界归一）。`None` = 缺失，需防御处理。
    pub finish_reason: Option<FinishReason>,
    /// 是否有可见正文（content 非空，或 reasoning-only 已在边界提升为正文）。
    pub has_visible_text: bool,
    /// 是否存在"工具信封"（```json 围栏 / 工具协议 tag）但解析失败（仅解析器置位）。
    pub malformed_envelope: bool,
    /// 该模型是否支持原生函数调用（`ModelConfig.native_tool_calling`）。
    pub native_tool_calling: bool,
    /// 本 run 是否已执行过工具（区分"跑过但没收尾" vs "啥也没跑"）。
    pub ran_tools: bool,
    /// 一次性 malformed 严格重试是否已用过。
    pub malformed_retry_done: bool,
}

/// 轮次决策（全枚举；主循环对其做 total match）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    /// 执行已解析的工具调用，然后继续循环（长任务续跑的核心驱动）。
    RunTools,
    /// 可见正文即最终回答，本轮终止（信任模型的文本决定；治愈点）。
    Answer,
    /// 提示模式下信封损坏且无可用正文 → 一次性严格 JSON 重试。
    RetryMalformed,
    /// 正文与调用皆空 → 交恢复层（跑过工具则合成活动摘要，否则类型化错误/请求总结）。
    Recover { ran_tools: bool },
    /// 被 max_tokens 截断且无可恢复调用/正文 → 走既有截断处理，绝不重提示。
    Truncated,
}

/// 单一决策入口：**只由结构信号推导**，不扫描任何正文子串。
///
/// 优先级（全枚举、无歧义、回归安全）：
/// 1. 有解析出的工具调用 → [`TurnDecision::RunTools`]（最高优先，保长任务续跑）。
/// 2. 无可见正文时：
///    a. `finish_reason == Length` → [`TurnDecision::Truncated`]（截断优先于重试：
///       microclaw 明确把 max_tokens 排除在"重问"之外——重问只会白烧预算）。
///    b. 坏信封 + 提示模式 + 未重试过 → [`TurnDecision::RetryMalformed`]（唯一的一次性推动）。
///    c. 其余 → [`TurnDecision::Recover`]（真正空响应，交恢复层）。
/// 3. 有可见正文且无调用 → [`TurnDecision::Answer`]（信任文本；即便 finish_reason=Length
///    也视为"部分回答"而非截断丢弃，保持与旧行为一致、不回退）。
pub fn decide_turn(s: &TurnSignals) -> TurnDecision {
    // 1) 真实工具调用永远最高优先——保证长任务/大 TODO 的工具推理不被打断。
    if s.tool_calls > 0 {
        return TurnDecision::RunTools;
    }
    // 2) 无可见正文
    if !s.has_visible_text {
        // 2a) 截断：交由既有截断修复/部分处理，绝不重提示。
        if s.finish_reason == Some(FinishReason::Length) {
            return TurnDecision::Truncated;
        }
        // 2b) 提示模式 + 真信封损坏 + 未重试 → 一次性严格重试（temm1e/zeroclaw 窄门）。
        if s.malformed_envelope && !s.native_tool_calling && !s.malformed_retry_done {
            return TurnDecision::RetryMalformed;
        }
        // 2c) 真正空响应 → 恢复层（thclaws：跑过工具则合成摘要，否则错误）。
        return TurnDecision::Recover {
            ran_tools: s.ran_tools,
        };
    }
    // 3) 有可见正文、无工具调用 → 信任为最终回答（治愈 HARTSAS 类误触）。
    TurnDecision::Answer
}

/// 结构探测：文本是否**包含一个工具调用信封**（``` 围栏内含 tool-call 键，或工具协议 tag）。
///
/// 仅认**结构标记**（代码围栏 / 协议 tag + 调用键名），**绝不**因散文提到工具名而置真——
/// 这正是 D3 与旧散文启发式的根本区别。用于喂 [`TurnSignals::malformed_envelope`]：当解析出的
/// `tool_calls` 为空但本函数为真，说明“有信封但解析失败”（潜在丢失的工具调用）。
///
/// 盲点：这是**宽松**探测——围栏与键名在全文范围匹配，未严格限定“键必须在围栏体内”，
/// 因此正文里展示 `{"name":..,"arguments":..}` 示例的文档可能误报。当前仅用于 `warn!` 观测，
/// 误报可接受；若将来据此触发重试，需收紧为“仅围栏体内”。
pub fn looks_like_tool_envelope(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    // 工具协议 tag（AntML / 常见变体）
    if t.contains("<tool_call") || t.contains("<function_call") || t.contains("<|tool_call") {
        return true;
    }
    // ``` 围栏 + tool-call 键组合
    if !t.contains("```") {
        return false;
    }
    (t.contains("\"name\"") && t.contains("\"arguments\""))
        || t.contains("\"tool_call\"")
        || (t.contains("\"function\"") && t.contains("\"name\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷构造：默认 = 提示模式、无调用、无正文、Stop、未跑工具、未重试。
    fn sig() -> TurnSignals {
        TurnSignals {
            finish_reason: Some(FinishReason::Stop),
            ..Default::default()
        }
    }

    // ── 1) RunTools：长任务续跑的核心保证 ──────────────────────────────

    #[test]
    fn run_tools_when_calls_present() {
        let s = TurnSignals { tool_calls: 2, ..sig() };
        assert_eq!(decide_turn(&s), TurnDecision::RunTools);
    }

    #[test]
    fn run_tools_beats_text_and_finish_reason() {
        // 即便同时有正文、finish=Stop，只要有解析出的调用就跑工具。
        let s = TurnSignals {
            tool_calls: 1,
            has_visible_text: true,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::RunTools);
    }

    #[test]
    fn run_tools_even_when_length_truncated() {
        // 截断但已修复出一个调用 → 仍跑工具（不因 length 而丢弃可用调用）。
        let s = TurnSignals {
            tool_calls: 1,
            finish_reason: Some(FinishReason::Length),
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::RunTools);
    }

    #[test]
    fn long_task_mid_turn_keeps_running() {
        // 长任务中途：已跑过工具 + 本轮又有调用 → 必须继续 RunTools（续跑保证）。
        let s = TurnSignals {
            tool_calls: 1,
            ran_tools: true,
            has_visible_text: true,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::RunTools);
    }

    // ── 2) Answer：治愈点（文本即终止，绝不因提到工具名而重提示）─────────

    #[test]
    fn decide_turn_hartsas_no_false_reprompt() {
        // HARTSAS 回归：native 模型、正文复述含 "browser_cdp"、无 tool_calls。
        // 旧启发式会因 mentions_tool 命中而强制重提示；新核必须判 Answer。
        // 注意：正文含工具名这一事实**根本不进入 TurnSignals**——结构上无法误判。
        let s = TurnSignals {
            native_tool_calling: true,
            has_visible_text: true,
            tool_calls: 0,
            malformed_envelope: false,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Answer);
    }

    #[test]
    fn answer_prompted_valid_response_without_tool_call() {
        // 提示模式：模型返回 {"response":"..."}（无 tool_call）→ 合法文本回答。
        let s = TurnSignals {
            native_tool_calling: false,
            has_visible_text: true,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Answer);
    }

    #[test]
    fn decide_turn_partial_text_is_answer() {
        // finish=length 但有部分正文、无调用 → Answer（部分回答不丢，与旧行为一致）。
        let s = TurnSignals {
            has_visible_text: true,
            finish_reason: Some(FinishReason::Length),
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Answer);
    }

    #[test]
    fn answer_when_finish_reason_missing_but_text_present() {
        // 防御：finish_reason 缺失 + 有正文 + 无调用 → Answer。
        let s = TurnSignals {
            has_visible_text: true,
            finish_reason: None,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Answer);
    }

    // ── 3) Truncated：截断优先于重试，绝不重提示 ────────────────────────

    #[test]
    fn decide_turn_truncation_not_reprompt() {
        let s = TurnSignals {
            has_visible_text: false,
            finish_reason: Some(FinishReason::Length),
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Truncated);
    }

    #[test]
    fn truncated_takes_precedence_over_malformed_retry() {
        // length + 坏信封 + 提示模式：截断优先，不重问（重问只烧预算）。
        let s = TurnSignals {
            has_visible_text: false,
            finish_reason: Some(FinishReason::Length),
            malformed_envelope: true,
            native_tool_calling: false,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::Truncated);
    }

    // ── 4) RetryMalformed：唯一的一次性推动，窄门控 ─────────────────────

    #[test]
    fn decide_turn_malformed_one_shot_prompted_only() {
        let s = TurnSignals {
            has_visible_text: false,
            malformed_envelope: true,
            native_tool_calling: false,
            malformed_retry_done: false,
            ..sig()
        };
        assert_eq!(decide_turn(&s), TurnDecision::RetryMalformed);
    }

    #[test]
    fn no_retry_when_native_tool_calling() {
        // 原生模型永不进入 RetryMalformed → 空响应走 Recover。
        let s = TurnSignals {
            has_visible_text: false,
            malformed_envelope: true,
            native_tool_calling: true,
            ..sig()
        };
        assert_eq!(
            decide_turn(&s),
            TurnDecision::Recover { ran_tools: false }
        );
    }

    #[test]
    fn no_retry_second_time() {
        // 已重试过 → 不再重试（一次性），转 Recover。
        let s = TurnSignals {
            has_visible_text: false,
            malformed_envelope: true,
            native_tool_calling: false,
            malformed_retry_done: true,
            ..sig()
        };
        assert_eq!(
            decide_turn(&s),
            TurnDecision::Recover { ran_tools: false }
        );
    }

    // ── 5) Recover：真空响应，区分是否跑过工具 ──────────────────────────

    #[test]
    fn decide_turn_empty_recover_distinguishes_ran_tools() {
        let ran = TurnSignals {
            has_visible_text: false,
            ran_tools: true,
            ..sig()
        };
        assert_eq!(decide_turn(&ran), TurnDecision::Recover { ran_tools: true });

        let fresh = TurnSignals {
            has_visible_text: false,
            ran_tools: false,
            ..sig()
        };
        assert_eq!(
            decide_turn(&fresh),
            TurnDecision::Recover { ran_tools: false }
        );
    }

    #[test]
    fn recover_when_finish_reason_missing_and_empty() {
        // 防御：finish_reason 缺失 + 无正文 + 无调用 + 无坏信封 → Recover（非 Truncated）。
        let s = TurnSignals {
            has_visible_text: false,
            finish_reason: None,
            ..sig()
        };
        assert_eq!(
            decide_turn(&s),
            TurnDecision::Recover { ran_tools: false }
        );
    }

    // ── 6) FinishReason 归一化 ─────────────────────────────────────────

    #[test]
    fn finish_reason_normalize_aliases() {
        assert_eq!(FinishReason::normalize(Some("stop")), Some(FinishReason::Stop));
        assert_eq!(FinishReason::normalize(Some("end_turn")), Some(FinishReason::Stop));
        assert_eq!(FinishReason::normalize(Some("STOP")), Some(FinishReason::Stop));
        assert_eq!(FinishReason::normalize(Some("tool_calls")), Some(FinishReason::ToolCalls));
        assert_eq!(FinishReason::normalize(Some("tool_use")), Some(FinishReason::ToolCalls));
        assert_eq!(FinishReason::normalize(Some("function_call")), Some(FinishReason::ToolCalls));
        assert_eq!(FinishReason::normalize(Some("length")), Some(FinishReason::Length));
        assert_eq!(FinishReason::normalize(Some("max_tokens")), Some(FinishReason::Length));
        assert_eq!(FinishReason::normalize(Some("content_filter")), Some(FinishReason::Other));
    }

    #[test]
    fn finish_reason_normalize_missing() {
        assert_eq!(FinishReason::normalize(None), None);
        assert_eq!(FinishReason::normalize(Some("")), None);
        assert_eq!(FinishReason::normalize(Some("   ")), None);
    }

    // ── 7) 全枚举可达性（每个决策都有确定路径）──────────────────────────

    #[test]
    fn every_decision_is_reachable() {
        assert_eq!(
            decide_turn(&TurnSignals { tool_calls: 1, ..sig() }),
            TurnDecision::RunTools
        );
        assert_eq!(
            decide_turn(&TurnSignals { has_visible_text: true, ..sig() }),
            TurnDecision::Answer
        );
        assert_eq!(
            decide_turn(&TurnSignals {
                malformed_envelope: true,
                ..sig()
            }),
            TurnDecision::RetryMalformed
        );
        assert_eq!(
            decide_turn(&TurnSignals {
                finish_reason: Some(FinishReason::Length),
                ..sig()
            }),
            TurnDecision::Truncated
        );
        assert_eq!(decide_turn(&sig()), TurnDecision::Recover { ran_tools: false });
    }

    // ── 8) looks_like_tool_envelope：信封结构探测（只认结构，不认散文）──

    #[test]
    fn envelope_json_fence_detected() {
        assert!(looks_like_tool_envelope(
            "```json\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ipconfig\"}}\n```"
        ));
    }

    #[test]
    fn envelope_tag_detected() {
        assert!(looks_like_tool_envelope("<tool_call>shell_exec<tool_call>"));
    }

    #[test]
    fn fence_with_tool_call_key_detected() {
        assert!(looks_like_tool_envelope(
            "```json\n{\"response\":\"x\",\"tool_call\":{\"name\":\"a\"}}\n```"
        ));
    }

    #[test]
    fn prose_mentioning_tool_is_not_envelope() {
        // HARTSAS 关键：复述含工具名的散文（无围栏）→ 绝不算信封。
        assert!(!looks_like_tool_envelope(
            "用 browser_cdp 模拟访问、追踪跳转。shell_exec 可运行命令。"
        ));
    }

    #[test]
    fn plain_code_fence_is_not_envelope() {
        assert!(!looks_like_tool_envelope("```rust\nfn main() {}\n```"));
    }

    #[test]
    fn empty_and_plain_text_are_not_envelope() {
        assert!(!looks_like_tool_envelope(""));
        assert!(!looks_like_tool_envelope("just a normal answer with no fence"));
    }
}
