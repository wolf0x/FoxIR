//! 深层记忆（Deep Memory）— 永久记忆打分与生命周期。
//!
//! 纯确定性核心：只做“这条事实多持久 / 属于哪一档”的数学，**无 LLM 调用、无 I/O**。
//! 模型：importance(I, 存储) + i_eff(惰性退火, 派生)。参考 Bjork / FSRS 调度。
//! 见 `output/memory-two-tier-spec.md` §3。

use serde::{Deserialize, Serialize};

/// 重要度范围（永久钳制，防多年漂移/饱和）。
pub const IMPORTANCE_MIN: f32 = 0.0;
pub const IMPORTANCE_MAX: f32 = 5.0;

#[inline]
fn clip(x: f32) -> f32 {
    x.clamp(IMPORTANCE_MIN, IMPORTANCE_MAX)
}

/// Current unix time in seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip `<memory>...</memory>` blocks from assistant text before displaying it
/// to the user (used by the write path).
pub fn strip_memory_blocks(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<memory>") {
        if let Some(rel) = result[start..].find("</memory>") {
            result.replace_range(start..start + rel + 9, "");
        } else {
            break;
        }
    }
    result.trim().to_string()
}

/// Rough token estimate (chars/4, rounded up, +1), used by deep_permanent_block
/// budgeting.
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4) + 1
}

/// 深层记忆打分参数。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DeepParams {
    /// 每次新 judgment 的 EMA 权重（钳制在 0..=1）。
    pub eta: f32,
    /// 提升阈值：i_eff ≥ theta_up ⇒ Permanent。
    pub theta_up: f32,
    /// 降级阈值：已是 Permanent 且 i_eff ≤ theta_down 才降档。
    pub theta_down: f32,
    /// 退火时间常数(天)；半衰期 ≈ tau_days·ln2。
    pub tau_days: f32,
    /// i_eff 低于此值 → Archived（仅留 hash）。
    pub archive_eps: f32,
}

impl Default for DeepParams {
    fn default() -> Self {
        Self {
            eta: 0.4,
            theta_up: 3.5,
            theta_down: 2.0,
            tau_days: 60.0,
            archive_eps: 0.05,
        }
    }
}

/// 记忆分层（由 effective importance 派生）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    /// 常驻注入（受 P_max 上限）；深层永久层。
    Permanent,
    /// 衰减的 λ 层；可见但非永久。
    Active,
    /// 淡出到 hash；按需召回。
    Archived,
}

/// 用首次 judgment 播种 importance。
/// 置信的 judgment(≥theta_up) 首次写入即永久——“asserted once is enough”。
pub fn seed(importance_hat: f32) -> f32 {
    clip(importance_hat)
}

/// EMA 更新：将当前 importance 与新的 judgment 混合。
/// 平滑(加上 tier 滞回)防止逐轮抖动。
pub fn ema_update(current: f32, importance_hat: f32, eta: f32) -> f32 {
    let eta = eta.clamp(0.0, 1.0);
    clip((1.0 - eta) * current + eta * importance_hat)
}

/// 有效重要度（惰性时间退火）。
/// `now`/`last_accessed` 为 unix 秒；用 saturating_sub 防时钟回拨通胀。
/// 用户 pin 的事实永不退火（i_eff = IMPORTANCE_MAX）。
pub fn effective_importance(
    importance: f32,
    last_accessed: u64,
    now: u64,
    pinned_by_user: bool,
    tau_days: f32,
) -> f32 {
    if pinned_by_user {
        return IMPORTANCE_MAX;
    }
    let dt_days = (now.saturating_sub(last_accessed) as f32) / 86_400.0;
    let tau = tau_days.max(f32::EPSILON);
    clip(importance * (-dt_days / tau).exp())
}

/// 由有效重要度派生档位，相对当前档位**应用滞回**，使悬在 [theta_down, theta_up]
/// 区间内的分数不抖动。用户 pin 恒为 Permanent。
pub fn tier(i_eff: f32, current: Tier, pinned_by_user: bool, p: &DeepParams) -> Tier {
    if pinned_by_user {
        return Tier::Permanent;
    }
    match current {
        // 粘性：保持 Permanent，直到落到(更低的)降级阈值。
        Tier::Permanent => {
            if i_eff > p.theta_down {
                Tier::Permanent
            } else if i_eff < p.archive_eps {
                Tier::Archived
            } else {
                Tier::Active
            }
        }
        // 尚未永久：只在(更高的)提升阈值上才升为 Permanent。
        Tier::Active | Tier::Archived => {
            if i_eff >= p.theta_up {
                Tier::Permanent
            } else if i_eff < p.archive_eps {
                Tier::Archived
            } else {
                Tier::Active
            }
        }
    }
}

/// 是否应出现在常驻永久块中（纯派生，无需落盘档位）。
/// 用户 pin 恒显示；agent pin 显示到退火至降级底线；未 pin 仅当跨过提升阈值。
pub fn is_visible_permanent(
    i_eff: f32,
    pinned_user: bool,
    pinned_agent: bool,
    p: &DeepParams,
) -> bool {
    pinned_user || (pinned_agent && i_eff > p.theta_down) || i_eff >= p.theta_up
}

/// 贪心预算打包（Permanent 块用，受 P_max 上限）。
/// 输入 `(i_eff, token_cost)`，按 i_eff 降序装到 budget 耗尽。返回索引（降序）。
pub fn pack_by_budget(items: &[(f32, usize)], budget: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| {
        items[b]
            .0
            .partial_cmp(&items[a].0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut used = 0usize;
    let mut out = Vec::new();
    for i in idx {
        let cost = items[i].1;
        if used + cost <= budget {
            used += cost;
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: DeepParams = DeepParams {
        eta: 0.4,
        theta_up: 3.5,
        theta_down: 2.0,
        tau_days: 60.0,
        archive_eps: 0.05,
    };
    const DAY: u64 = 86_400;

    #[test]
    fn seed_clamps() {
        assert_eq!(seed(4.0), 4.0);
        assert_eq!(seed(9.0), 5.0);
        assert_eq!(seed(-1.0), 0.0);
    }

    #[test]
    fn ema_moves_toward_target_and_clamps() {
        let a = ema_update(0.0, 5.0, 0.4); // 2.0
        assert!((a - 2.0).abs() < 1e-5);
        let b = ema_update(a, 5.0, 0.4); // 0.6*2 + 0.4*5 = 3.2
        assert!((b - 3.2).abs() < 1e-5);
        assert_eq!(ema_update(5.0, 10.0, 1.0), 5.0);
        assert_eq!(ema_update(0.0, 4.0, 0.0), 0.0);
    }

    #[test]
    fn effective_pin_is_max() {
        assert_eq!(effective_importance(0.1, 0, 10_000_000, true, 60.0), 5.0);
    }

    #[test]
    fn effective_anneals_over_time() {
        let now = 100 * DAY;
        let v = effective_importance(4.0, now - 60 * DAY, now, false, 60.0);
        assert!((v - 4.0 * std::f32::consts::E.recip()).abs() < 0.05, "v={v}");
    }

    #[test]
    fn effective_clock_skew_guarded() {
        let v = effective_importance(3.0, 1000, 500, false, 60.0);
        assert!((v - 3.0).abs() < 1e-4);
    }

    #[test]
    fn tier_hysteresis_is_sticky() {
        assert_eq!(tier(3.0, Tier::Permanent, false, &P), Tier::Permanent);
        assert_eq!(tier(3.0, Tier::Active, false, &P), Tier::Active);
        assert_eq!(tier(1.9, Tier::Permanent, false, &P), Tier::Active);
    }

    #[test]
    fn tier_archives_below_eps() {
        assert_eq!(tier(0.01, Tier::Active, false, &P), Tier::Archived);
        assert_eq!(tier(0.01, Tier::Permanent, false, &P), Tier::Archived);
    }

    #[test]
    fn tier_pin_always_permanent() {
        assert_eq!(tier(0.0, Tier::Archived, true, &P), Tier::Permanent);
    }

    #[test]
    fn visible_permanent_rules() {
        assert!(is_visible_permanent(0.0, true, false, &P));
        assert!(is_visible_permanent(3.0, false, true, &P));
        assert!(!is_visible_permanent(1.5, false, true, &P));
        assert!(is_visible_permanent(3.6, false, false, &P));
        assert!(!is_visible_permanent(3.0, false, false, &P));
    }

    #[test]
    fn pack_respects_budget_highest_first() {
        let items = [(1.0, 50), (5.0, 40), (3.0, 40)];
        assert_eq!(pack_by_budget(&items, 80), vec![1, 2]);
    }
}

// ── 深层(Deep)数据模型（纯数据，无 I/O）──────────────────────────

/// 谁 pin 了这条事实。`User` 神圣——永不退火、永不自动降档。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PinnedBy {
    #[default]
    None,
    Agent,
    User,
}

/// 可见范围（防止跨会话/跨聊天泄漏）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum MemoryScope {
    #[default]
    Global,
    User(String),
    Chat(String),
}

impl MemoryScope {
    pub fn as_key(&self) -> String {
        match self {
            MemoryScope::Global => "global".to_string(),
            MemoryScope::User(u) => format!("user:{u}"),
            MemoryScope::Chat(c) => format!("chat:{c}"),
        }
    }
    pub fn from_key(s: &str) -> Self {
        if let Some(u) = s.strip_prefix("user:") {
            MemoryScope::User(u.to_string())
        } else if let Some(c) = s.strip_prefix("chat:") {
            MemoryScope::Chat(c.to_string())
        } else {
            MemoryScope::Global
        }
    }
}

/// 语义类别（驱动渲染分组）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FactType {
    Identity,
    Preference,
    Project,
    Constraint,
    #[default]
    Reference,
}

impl FactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactType::Identity => "identity",
            FactType::Preference => "preference",
            FactType::Project => "project",
            FactType::Constraint => "constraint",
            FactType::Reference => "reference",
        }
    }
}

/// 一条深层记忆（永久层）事实。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepFact {
    pub id: String,
    pub content: String,
    pub summary: String,
    pub essence: String,
    pub fact_type: FactType,
    pub scope: MemoryScope,
    pub pinned_by: PinnedBy,
    /// 同主题覆盖键：同 subject_key 的新事实替换旧事实。
    pub subject_key: Option<String>,
    /// 存储 importance `I ∈ [0,5]`。
    pub importance: f32,
    pub created_at: u64,
    pub last_accessed: u64,
    pub tags: Vec<String>,
    pub links: Vec<String>,
}

impl DeepFact {
    /// 统一工件价值 `V = Q² · R · U`（实现在 crate::value）。
    /// Q = 归一化重要度(importance/5)；R = 距 last_accessed 的陈旧衰减（默认半衰期）；
    /// U = 使用促进。深层为永久层、不追踪访问次数，U 取 1（仅依赖 Q×R 排序）。
    /// 用于在上下文预算内挑选“最有价值”的常驻事实（见 memory::deep_permanent_block）。
    pub fn value(&self, now: u64) -> f64 {
        let q = crate::value::quality_from_importance(self.importance, IMPORTANCE_MAX);
        crate::value::unified_value(
            q,
            1,
            self.last_accessed,
            now,
            crate::value::DEFAULT_HALF_LIFE_DAYS,
        )
    }
}
