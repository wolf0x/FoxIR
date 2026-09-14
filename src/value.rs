//! value — 统一工件价值函数  V = Q² × R × U。
//!
//! 单一数学框架评估**所有学习产物**（SOP/蓝图、深层记忆、经验），
//! 确保上下文窗口在预算内始终保留最有价值的信息。纯确定性、无 I/O、无 LLM。
//!
//!   V(a, t) = Q(a) × R(a, t) × U(a)
//!     - Q = 质量/置信 ∈ [0,1]：SOP 用 Wilson 99% 下限；记忆用归一化重要度
//!     - R = 陈旧衰减 exp(-ln2·天/半衰期)，半衰期默认 139 天
//!     - U = 使用促进 1 + 0.5·ln(1 + 使用次数)
//!
//! 本文件是唯一实现；各工件类型通过 `unified_value` / `learning_value` 接入。

use std::time::{SystemTime, UNIX_EPOCH};

/// 统一框架默认半衰期（天）—— 对应 temm1e 的 139 天。
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 139.0;
/// 从未执行/无数据时的保守先验（避免把“没用过”当“很差”）。
pub const UNINFORMED_PRIOR: f64 = 0.5;
/// Wilson 99% 置信的 z 值。
const Z_99: f64 = 2.576;

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Q：质量/置信 ──────────────────────────────────────────────

/// Wilson 区间下限（置信水平由 z 决定，默认 99%）。总次数为 0 时返回保守先验。
pub fn wilson_lower(successes: u32, total: u32, z: f64) -> f64 {
    if total == 0 {
        return UNINFORMED_PRIOR;
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = z * z;
    let denom = n + z2;
    let center = (n * p + z2 / 2.0) / denom;
    let margin = z * ((n * p * (1.0 - p) + z2 / 4.0) / (denom * denom)).sqrt();
    (center - margin).max(0.0)
}

/// 把重要度 [0, max] 归一化为质量 [0,1]（用于记忆类工件）。
pub fn quality_from_importance(importance: f32, max_importance: f32) -> f64 {
    ((importance as f64) / (max_importance.max(1e-6) as f64)).clamp(0.0, 1.0)
}

// ── R：陈旧衰减 ───────────────────────────────────────────────

/// `exp(-ln2 · 天数 / 半衰期)`；fresh ⇒ 1，随久未使用指数下降。`last_used` 取 saturating。
pub fn freshness_decay(last_used: u64, now: u64, half_life_days: f64) -> f64 {
    let days = now.saturating_sub(last_used) as f64 / 86_400.0;
    (-std::f64::consts::LN_2 * days / half_life_days.max(1e-6)).exp()
}

// ── U：使用促进 ───────────────────────────────────────────────

/// `1 + 0.5·ln(1 + usage_count)`；用得越频繁越靠前。
pub fn usage_boost(usage_count: u32) -> f64 {
    1.0 + 0.5 * (1.0 + usage_count as f64).ln()
}

// ── 统一价值 ─────────────────────────────────────────────────

/// `V = Q² · R · U`。quality ∈ [0,1]；half_life_days 可依工件类型挑选（框架统一）。
pub fn unified_value(
    quality: f64,
    usage_count: u32,
    last_used: u64,
    now: u64,
    half_life_days: f64,
) -> f64 {
    let q = quality.clamp(0.0, 1.0);
    let r = freshness_decay(last_used, now, half_life_days);
    let u = usage_boost(usage_count);
    (q * q) * r * u
}

/// SOP / 经验条目的“结果导向”价值：Q 由 Wilson(成功率) 计算，U 取执行次数，默认半衰期。
pub fn learning_value(
    successes: u32,
    executed: u32,
    last_used: Option<u64>,
    now: u64,
) -> f64 {
    let q = wilson_lower(successes, executed, Z_99);
    let last = last_used.unwrap_or(now);
    unified_value(q, executed, last, now, DEFAULT_HALF_LIFE_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    const DAY: u64 = 86_400;

    #[test]
    fn wilson_prior_when_untried() {
        assert_eq!(wilson_lower(0, 0, 2.576), UNINFORMED_PRIOR);
    }
    #[test]
    fn wilson_penalizes_low_success() {
        assert!(wilson_lower(2, 10, 2.576) < wilson_lower(8, 10, 2.576));
    }
    #[test]
    fn quality_normalizes_importance() {
        assert!((quality_from_importance(5.0, 5.0) - 1.0).abs() < 1e-9);
        assert!((quality_from_importance(0.0, 5.0)).abs() < 1e-9);
        assert!((quality_from_importance(2.5, 5.0) - 0.5).abs() < 1e-9);
    }
    #[test]
    fn freshness_decays_with_age() {
        let now = 50_000_000;
        assert!((freshness_decay(now, now, 139.0) - 1.0).abs() < 1e-9);
        let stale = freshness_decay(now - 139 * DAY, now, 139.0);
        assert!((stale - 0.5).abs() < 1e-3, "half-life should halve: {stale}");
    }
    #[test]
    fn usage_boost_rewards_use() {
        assert!(usage_boost(0) < usage_boost(10));
        assert!(usage_boost(0) > 0.0);
    }
    #[test]
    fn unified_value_orders_by_expected_directions() {
        let now = 50_000_000;
        let fresh_hi = unified_value(1.0, 10, now, now, 139.0);
        let fresh_lo = unified_value(0.2, 10, now, now, 139.0);
        assert!(fresh_lo < fresh_hi);
        let stale_hi = unified_value(1.0, 10, now - 300 * DAY, now, 139.0);
        assert!(stale_hi < fresh_hi);
        let fresh_low_use = unified_value(1.0, 1, now, now, 139.0);
        assert!(fresh_low_use < fresh_hi);
    }
    #[test]
    fn learning_value_uses_composition() {
        let now = 100 * DAY;
        let weak = learning_value(2, 10, Some(now), now);
        let strong = learning_value(8, 10, Some(now), now);
        assert!(weak < strong);
    }
}
