//! 執行期設定的純規則（B 期，lead 2026-11 定案）：角色 policy（`projects.role_policy_json`）與 bot 執行期覆寫
//! （`bots.runtime_override_json`）同一個形狀，生效順序 **bot 覆寫 > 池條目 > 角色 policy > 預設**。
//! 這裡只放形狀、合併與保溫參數換算；驗證（context 視窗、TTL 上限等要看 kind／model 的）在 daemon。
//! 欄位 `null`＝這一層不管、往下一層找；全部 null＝預設（現有行為，不偷偷幫誰開保溫）。

use serde::{Deserialize, Serialize};

/// 一層設定（角色 policy 的一個 role，或 bot 覆寫）。未知欄位在解析時就拒絕（`deny_unknown_fields`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    /// 啟動時寫進 context 上限的 token 數；`None`＝跟上一層／模型預設。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// `Some(true)`：這顆（claude）即使不是主力也保溫；`Some(false)`：主力也不保溫；`None`＝跟主力預設。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_warm: Option<bool>,
    /// 閒置多久（分）後保溫。5..=59 且 < TTL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_warm_after_min: Option<i64>,
    /// 閒置多久（分）後熱壓（> keep_warm_after_min）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_compact_after_min: Option<i64>,
    /// context 用量超過幾 % 才熱壓（0..=100）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_compact_min_pct: Option<i64>,
}

/// 合併後的結果：每個欄位 `None`＝沒有任何一層設定它（用預設）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effective {
    pub context_tokens: Option<u32>,
    pub keep_warm: Option<bool>,
    pub keep_warm_after_min: Option<i64>,
    pub hot_compact_after_min: Option<i64>,
    pub hot_compact_min_pct: Option<i64>,
}

/// 四層合併。`pool_context` 只給 context（池條目只有 context_tokens；保溫欄位池條目不管）。
/// 每個欄位取第一個有值的：bot 覆寫 → 池條目（僅 context）→ 角色 policy。
pub fn effective(bot_override: Option<&RuntimeSettings>, pool_context: Option<u32>, role: Option<&RuntimeSettings>) -> Effective {
    fn pick<T: Clone>(f: impl Fn(&RuntimeSettings) -> Option<T>, ov: Option<&RuntimeSettings>, role: Option<&RuntimeSettings>) -> Option<T> {
        ov.and_then(&f).or_else(|| role.and_then(&f))
    }
    Effective {
        context_tokens: bot_override.and_then(|o| o.context_tokens).or(pool_context).or_else(|| role.and_then(|r| r.context_tokens)),
        keep_warm: pick(|s| s.keep_warm, bot_override, role),
        keep_warm_after_min: pick(|s| s.keep_warm_after_min, bot_override, role),
        hot_compact_after_min: pick(|s| s.hot_compact_after_min, bot_override, role),
        hot_compact_min_pct: pick(|s| s.hot_compact_min_pct, bot_override, role),
    }
}

/// 保溫／熱壓真正用的門檻（秒與百分比），缺的用現有常數。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeepWarmTuning {
    /// `Some(true)`／`Some(false)`＝強制開／關；`None`＝跟主力預設。
    pub enabled: Option<bool>,
    pub keep_warm_after_secs: i64,
    pub warm_compact_after_secs: i64,
    pub min_context_pct: Option<f64>,
}

impl Effective {
    /// 換成保溫門檻。`default_min_pct` 是這個 kind 原本的熱壓門檻（claude 30、其他 None）。
    /// 分鐘值只在 5..=59（保溫）與 > 保溫分鐘（熱壓）時才採用，否則退回預設（驗證已擋，這裡是保險）。
    pub fn keep_warm_tuning(&self, default_keep_warm_secs: i64, default_compact_secs: i64, default_min_pct: Option<f64>) -> KeepWarmTuning {
        let keep = self.keep_warm_after_min.filter(|m| (5..=59).contains(m)).map(|m| m * 60).unwrap_or(default_keep_warm_secs);
        let compact = self
            .hot_compact_after_min
            .filter(|m| *m > keep / 60)
            .map(|m| m * 60)
            .unwrap_or(default_compact_secs);
        KeepWarmTuning {
            enabled: self.keep_warm,
            keep_warm_after_secs: keep,
            warm_compact_after_secs: compact,
            min_context_pct: self.hot_compact_min_pct.map(|p| p as f64).or(default_min_pct),
        }
    }
}

/// 生效結果的指紋（啟動時記在 run 上；之後 policy 變了 bot 的 role_pending 就亮）。
pub fn policy_rev(eff: &Effective) -> String {
    let canon = serde_json::json!([eff.context_tokens, eff.keep_warm, eff.keep_warm_after_min, eff.hot_compact_after_min, eff.hot_compact_min_pct]);
    format!("{:016x}", crate::launch_rev::fnv1a64(&canon.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(json: &str) -> RuntimeSettings {
        serde_json::from_str(json).unwrap()
    }

    /// 四層：bot 覆寫 > 池條目 > 角色 policy > 預設（全部 None）。
    #[test]
    fn four_layers_resolve_in_order() {
        let role = s(r#"{"context_tokens": 200000, "keep_warm": true, "keep_warm_after_min": 30}"#);
        let ov = s(r#"{"context_tokens": 512000, "keep_warm_after_min": 40}"#);

        // 0. 預設：什麼都沒設＝全 None，保溫用現有常數。
        let none = effective(None, None, None);
        assert_eq!(none, Effective::default());
        let t = none.keep_warm_tuning(58 * 60, 110 * 60, Some(30.0));
        assert_eq!((t.enabled, t.keep_warm_after_secs, t.warm_compact_after_secs, t.min_context_pct), (None, 58 * 60, 110 * 60, Some(30.0)));

        // 1. 只有角色 policy。
        let r = effective(None, None, Some(&role));
        assert_eq!((r.context_tokens, r.keep_warm, r.keep_warm_after_min), (Some(200_000), Some(true), Some(30)));

        // 2. 池條目的 context 蓋過角色，但不蓋過保溫欄位。
        let p = effective(None, Some(128_000), Some(&role));
        assert_eq!(p.context_tokens, Some(128_000));
        assert_eq!(p.keep_warm_after_min, Some(30));

        // 3. bot 覆寫最大：context 與分鐘都以覆寫為準。
        let o = effective(Some(&ov), Some(128_000), Some(&role));
        assert_eq!((o.context_tokens, o.keep_warm, o.keep_warm_after_min), (Some(512_000), Some(true), Some(40)));

        // 覆寫沒寫的欄位往下找（keep_warm 來自角色）。
        let partial = s(r#"{"keep_warm": false}"#);
        assert_eq!(effective(Some(&partial), None, Some(&role)).keep_warm, Some(false));
    }

    #[test]
    fn tuning_uses_effective_values_and_falls_back_when_inconsistent() {
        let e = effective(None, None, Some(&s(r#"{"keep_warm_after_min": 30, "hot_compact_after_min": 50, "hot_compact_min_pct": 60}"#)));
        let t = e.keep_warm_tuning(58 * 60, 110 * 60, None);
        assert_eq!((t.keep_warm_after_secs, t.warm_compact_after_secs, t.min_context_pct), (30 * 60, 50 * 60, Some(60.0)));

        // 熱壓分鐘不大於保溫分鐘：退回預設（驗證擋掉，這裡保險）。
        let bad = effective(None, None, Some(&s(r#"{"keep_warm_after_min": 30, "hot_compact_after_min": 20}"#)));
        assert_eq!(bad.keep_warm_tuning(58 * 60, 110 * 60, None).warm_compact_after_secs, 110 * 60);
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_str::<RuntimeSettings>(r#"{"keep_warm": true, "bogus": 1}"#).is_err());
    }

    #[test]
    fn policy_rev_changes_with_effective_values_only() {
        let a = effective(None, None, Some(&s(r#"{"context_tokens": 200000}"#)));
        let same = effective(None, Some(200_000), None);
        assert_eq!(policy_rev(&a), policy_rev(&same), "同樣的生效值指紋相同");
        let b = effective(None, None, Some(&s(r#"{"context_tokens": 512000}"#)));
        assert_ne!(policy_rev(&a), policy_rev(&b));
    }
}
