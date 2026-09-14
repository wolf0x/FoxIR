//! 统一上下文仲裁器（Finite Brain 核心）— 有限工作记忆的价值导向装填。
//!
//! 纯确定性核：**无 I/O、无 LLM**，沿用 `deep_memory` 的纯核 + 全测模式。
//! 设计见 `output/SDD-Memory-SOP-Hardening.md` §12。
//!
//! 核心思想：上下文是有硬上限的工作记忆，每个 token 都是被征用的神经元。所有可注入
//! 产物（深层事实 / SOP / knowledge / skill / 历史轮次）统一按
//! `rank_key = value × (0.5 + relevance)` 排序，贪心装填到共享预算，超限时逐级优雅降级
//! （全文 → 提纲/指针 → 目录 → 丢弃），**绝不静默截断不可再生的证据**。
//!
//! 生产消费点：
//! - `agent::llm_agent::trim_history_by_value`（§12.6 价值导向裁剪）用 [`rank_key`]。
//! - `server.rs` §12.8 注入收敛用 [`assemble`]（块级）并把 [`budget_dashboard`]/[`budget_report`]
//!   接入系统提示与 Dashboard 页。逐条打分接入是后续精细化方向。

/// 可注入 / 可降级的上下文产物类别。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    DeepFact,
    Sop,
    Knowledge,
    Skill,
    /// 历史轮次（价值导向裁剪复用同一排序核）。
    HistoryTurn,
}

/// 三档可降级渲染。放得下 `full` 用 `full`，否则 `outline`（提纲 / 指针），
/// 再否则 `catalog`（一行目录），都放不下才跳过。空串表示该档不可用。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Render {
    pub full: String,
    pub outline: String,
    pub catalog: String,
}

/// 一个参与统一仲裁的上下文产物。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub id: String,
    /// 统一价值 V（Q²·R·U 语义，由各产物各自的打分派生；这里是已归一的标量）。
    pub value: f64,
    /// 任务相关性 [0,1]。
    pub relevance: f64,
    /// full / outline / catalog 各自的 token 预估。
    pub cost: [usize; 3],
    pub render: Render,
    /// 用户 pin / always / 不可再生证据：不可被整体淘汰，只可降级。
    pub pinned: bool,
}

/// [`assemble`] 的装填结果。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Assembled {
    /// 按装入顺序渲染好的文本块（已是各产物选定的降级档）。
    pub blocks: Vec<String>,
    /// 已占用 token 预估。
    pub used: usize,
    /// 为对话 / 工具结果预留而未参与学习产物装填的额度。
    pub reserved: usize,
}

/// 排序键 = 价值 ×（0.5 + 相关性）。
///
/// 关键：**不能只用裸 V**。V=Q²·R·U 不含任务相关性，只用 V 会「保住泛泛重要、丢掉当下关键」。
/// 0.5 基线保证泛重要产物仍有一席之地，高相关最多 ×1.5。见 §12.4 / §12.9。
#[inline]
pub fn rank_key(value: f64, relevance: f64) -> f64 {
    value.max(0.0) * (0.5 + relevance.clamp(0.0, 1.0))
}

/// 贪心装填 + 逐级降级。
///
/// - 预算先扣去 `reserve`（对话 / 工具结果预留），学习产物永不占满上下文。
/// - `pinned` 产物置顶且至少保留 `catalog`（可降级、不可整体淘汰）。
/// - 其余按 [`rank_key`] 降序，逐个尝试 full → outline → catalog → 跳过。
#[allow(dead_code)]
pub fn assemble(arts: &mut [Artifact], budget: usize, reserve: usize) -> Assembled {
    let cap = budget.saturating_sub(reserve);
    let reserved = reserve.min(budget);

    arts.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(
                rank_key(b.value, b.relevance)
                    .partial_cmp(&rank_key(a.value, a.relevance))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    let mut used = 0usize;
    let mut blocks: Vec<String> = Vec::with_capacity(arts.len());
    for a in arts.iter() {
        let tiers: [(&str, usize); 3] = [
            (a.render.full.as_str(), a.cost[0]),
            (a.render.outline.as_str(), a.cost[1]),
            (a.render.catalog.as_str(), a.cost[2]),
        ];
        let mut placed = false;
        for (text, cost) in tiers {
            if text.is_empty() {
                continue;
            }
            if used + cost <= cap {
                used += cost;
                blocks.push(text.to_string());
                placed = true;
                break;
            }
        }
        // pinned 且预算内三档都放不下：至少塞入 catalog（允许轻微超预算），绝不静默消失。
        if !placed && a.pinned {
            let cat = if !a.render.catalog.is_empty() {
                a.render.catalog.as_str()
            } else if !a.render.outline.is_empty() {
                a.render.outline.as_str()
            } else {
                a.render.full.as_str()
            };
            if !cat.is_empty() {
                used += a.cost[2].max(crate_cost(cat));
                blocks.push(cat.to_string());
            }
        }
    }

    Assembled { blocks, used, reserved }
}

/// 极小的预算仪表盘文本（约 40 token），让模型「知道自己的颅骨」以自我调节。
///
/// **不作为正确性依赖**（模型常忽略此类提示），硬仲裁始终是权威。见 §12.7 / §12.9。
#[allow(dead_code)]
pub fn budget_dashboard(window: usize, reserve: usize, used: usize, free: usize) -> String {
    format!(
        "=== CONTEXT BUDGET ===\nwindow {} | reserve {} | artifacts {} | free {}\n=== END ===",
        window, reserve, used, free
    )
}

/// 把一个「整块文本」产物适配为可参与仲裁的 `Artifact`（块级粒度）。
/// 当前各 block 构造器仍返回拼接好的整块文本，这里作为接入 `assemble` 的过渡：
/// 全文为 `full`，前 4 行为 `outline`，首行为 `catalog`；逐条打分留给后续改造。
#[allow(dead_code)]
pub fn artifact_from_block(
    kind: ArtifactKind,
    id: &str,
    value: f64,
    relevance: f64,
    pinned: bool,
    full: String,
) -> Artifact {
    let outline = full.lines().take(4).collect::<Vec<_>>().join("\n");
    let catalog = full.lines().next().unwrap_or("").chars().take(80).collect::<String>();
    let cost = [crate_cost(&full), crate_cost(&outline), crate_cost(&catalog)];
    Artifact {
        kind,
        id: id.to_string(),
        value,
        relevance,
        cost,
        render: Render { full, outline, catalog },
        pinned,
    }
}

/// 预算仪表盘的原始行数据（供 `/api/budget` 页渲染）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BudgetLine {
    pub category: String,
    pub tokens: usize,
}

/// 预算仪表盘的完整结构化报告（驱动 Dashboard 页的「有限脑」视图）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BudgetReport {
    pub window: usize,
    pub reserve: usize,
    pub used: usize,
    pub free: usize,
    pub lines: Vec<BudgetLine>,
}

/// 汇总预算报告：`window` 总窗口，`reserve` 预留，其余为各用途占用。
/// 供系统提示自省文本与 Dashboard 页共用同一份数据。
pub fn budget_report(window: usize, reserve: usize, usage: &[(&str, usize)]) -> BudgetReport {
    let lines: Vec<BudgetLine> = usage
        .iter()
        .filter(|(_, t)| *t > 0)
        .map(|(name, t)| BudgetLine { category: name.to_string(), tokens: *t })
        .collect();
    let used: usize = lines.iter().map(|l| l.tokens).sum();
    let free = window.saturating_sub(reserve + used);
    BudgetReport { window, reserve, used, free, lines }
}

/// 内部 token 预估（chars/4 上取整 +1）。
#[allow(dead_code)]
fn crate_cost(text: &str) -> usize {
    (text.chars().count() / 4) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(kind: ArtifactKind, id: &str, value: f64, relevance: f64, pinned: bool) -> Artifact {
        let full = format!("[{}:{}] full body text", id, kind as u8);
        let outline = format!("{} outline", id);
        let catalog = format!("{}", id);
        let cost = [crate_cost(&full), crate_cost(&outline), crate_cost(&catalog)];
        Artifact {
            kind,
            id: id.to_string(),
            value,
            relevance,
            cost,
            render: Render { full, outline, catalog },
            pinned,
        }
    }

    #[test]
    fn rank_key_blends_relevance() {
        // 同等价值下，高相关排更高；证明相关性确实进入排序键（非裸 V）。
        let hi = rank_key(1.0, 1.0);
        let lo = rank_key(1.0, 0.0);
        assert!(hi > lo);
        // relevance 被 clamp 到 [0,1]
        assert_eq!(rank_key(2.0, 5.0), rank_key(2.0, 1.0));
        assert_eq!(rank_key(2.0, -3.0), 2.0 * 0.5);
        // 负价值不产生负排序键
        assert_eq!(rank_key(-1.0, 0.5), 0.0);
    }

    #[test]
    fn arbiter_ranks_by_value_times_relevance() {
        // 低相关高 V vs 高相关低 V：rank_key 应让「当下关键」胜出。
        let generic_high_v = rank_key(5.0, 0.0); // 2.5
        let critical_low_v = rank_key(3.0, 1.0); // 4.5
        assert!(critical_low_v > generic_high_v);
    }

    #[test]
    fn assemble_prefers_higher_rank_into_full() {
        let mut arts = vec![
            art(ArtifactKind::HistoryTurn, "low", 1.0, 0.0, false),
            art(ArtifactKind::DeepFact, "high", 5.0, 1.0, false),
        ];
        let res = assemble(&mut arts, 1000, 0);
        // 两者都放得下，高 rank 的在前
        assert_eq!(res.blocks.len(), 2);
        assert!(res.blocks[0].contains("high"));
    }

    #[test]
    fn assemble_degrades_not_drops() {
        // 预算只够一个 full：第二个应降级为 outline/catalog 而非消失。
        let big = Artifact {
            kind: ArtifactKind::DeepFact,
            id: "big".into(),
            value: 5.0,
            relevance: 0.5,
            cost: [0, 0, 0],
            render: Render {
                full: "F".repeat(400),
                outline: "O".repeat(40),
                catalog: "C".repeat(4),
            },
            pinned: false,
        };
        let mut big = big;
        big.cost = [crate_cost(&big.render.full), crate_cost(&big.render.outline), crate_cost(&big.render.catalog)];
        let first = Artifact {
            kind: ArtifactKind::DeepFact,
            id: "first".into(),
            value: 9.0,
            relevance: 1.0,
            cost: [5, 5, 5],
            render: Render { full: "first-full".into(), outline: "first-out".into(), catalog: "first".into() },
            pinned: false,
        };
        let mut arts = vec![first, big];
        // cap=60：first-full(约3) 用掉约 3，big 的 full(约101) 放不下 → 降级到 outline(约11) 或 catalog(2)
        let res = assemble(&mut arts, 60, 0);
        assert!(!res.blocks.is_empty());
        // big 至少以某档存在（没有静默消失），除非预算真被占满
        assert!(res.blocks.iter().any(|b| b.contains('O') || b.contains('C') || b.contains("F")));
    }

    #[test]
    fn assemble_reserves_dialog_budget() {
        // reserve 部分永不分给学习产物。
        let arts = vec![art(ArtifactKind::Sop, "s", 1.0, 0.0, false)];
        let mut arts = arts;
        // 单个 artifact full 成本约 (len/4+1)
        let budget = 100;
        let reserve = 98;
        let res = assemble(&mut arts, budget, reserve);
        assert_eq!(res.reserved, 98);
        // 只剩 2 容量给产物 → full 放不下，可能降到 catalog
        assert!(res.used <= (budget - reserve) || res.blocks.iter().any(|b| b == "s"));
    }

    #[test]
    fn pinned_never_fully_dropped() {
        // pinned 产物即便预算紧张也要以 catalog 档存在。
        let mut pinned_art = art(ArtifactKind::DeepFact, "pin", 1.0, 0.0, true);
        pinned_art.cost = [crate_cost(&pinned_art.render.full), crate_cost(&pinned_art.render.outline), crate_cost(&pinned_art.render.catalog)];
        let mut arts = vec![pinned_art];
        let res = assemble(&mut arts, 1, 0); // 极小预算
        assert!(res.blocks.iter().any(|b| b.contains("pin")));
    }

    #[test]
    fn budget_dashboard_format() {
        let d = budget_dashboard(128000, 25600, 61200, 35648);
        assert!(d.contains("CONTEXT BUDGET"));
        assert!(d.contains("128000"));
    }

    #[test]
    fn budget_report_sums_and_free() {
        let r = budget_report(128000, 12800, &[("system", 16000), ("history", 32000)]);
        assert_eq!(r.window, 128000);
        assert_eq!(r.reserve, 12800);
        assert_eq!(r.used, 16000 + 32000);
        assert_eq!(r.free, 128000 - 12800 - (16000 + 32000));
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].category, "system");
    }

    #[test]
    fn budget_report_drops_zero_lines() {
        let r = budget_report(128000, 0, &[("system", 100), ("tools", 0), ("memory", 0)]);
        assert_eq!(r.lines.len(), 1);
        assert_eq!(r.lines[0].category, "system");
    }
}
