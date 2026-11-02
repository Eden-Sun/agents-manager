//! Anthropic cost_report 與帳本混合計算預付餘額（issue #1944）。
//!
//! 計算邏輯：
//! 使用者在時間點 T 設定 Console 餘額錨點 B（美元）。
//! 1. T 當天（UTC）：只使用本機帳 `api_bot_attempts`（`created_at >= T` 且 `created_at < T_next_day_utc`）
//!    的 recorded + unknown 花費（微美元轉美元）。因為當天的 cost_report 含 T 之前的花費，無法切割。
//! 2. T 隔天起、cost_report 已有的完整 UTC 日：使用 cost_report 金額（amount 單位為美分字串，÷100 轉為美元）。
//! 3. cost_report 尚未涵蓋的日子（通常是未結束的今天，或 cost_report 結束時間之後）：使用本機帳
//!    `created_at >= report_through` 的 recorded + unknown 花費。
//!
//! remaining = B - total_spent。

use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::lc_error::LcError;
use crate::state::App;

/// 快取過期時間：10 分鐘
const CACHE_TTL: Duration = Duration::from_secs(600);

/// cost_report 單日桶資料
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostReportBucket {
    pub starting_at: String,
    pub ending_at: String,
    pub results: Vec<CostReportResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostReportResult {
    pub currency: Option<String>,
    pub amount: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostReportResponse {
    pub data: Vec<CostReportBucket>,
    pub has_more: Option<bool>,
    pub next_page: Option<String>,
}

/// 記憶體中的快取狀態
#[derive(Default)]
pub struct BalanceCache {
    inner: Mutex<Option<CachedReport>>,
}

struct CachedReport {
    cached_at: Instant,
    starting_at_req: String,
    buckets: Vec<CostReportBucket>,
}

impl BalanceCache {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }

    async fn get(&self, starting_at: &str) -> Option<Vec<CostReportBucket>> {
        let guard = self.inner.lock().await;
        if let Some(ref c) = *guard {
            if c.starting_at_req == starting_at && c.cached_at.elapsed() < CACHE_TTL {
                return Some(c.buckets.clone());
            }
        }
        None
    }

    async fn set(&self, starting_at: String, buckets: Vec<CostReportBucket>) {
        let mut guard = self.inner.lock().await;
        *guard = Some(CachedReport {
            cached_at: Instant::now(),
            starting_at_req: starting_at,
            buckets,
        });
    }

    pub async fn invalidate(&self) {
        let mut guard = self.inner.lock().await;
        *guard = None;
    }
}

/// 解析 RFC3339 時間
fn parse_rfc3339(ts: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| anyhow!("invalid RFC3339 timestamp '{ts}': {e}"))
}

/// 取得某日 00:00:00 UTC
fn start_of_day(dt: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(dt.year(), dt.month(), dt.day(), 0, 0, 0).unwrap()
}

/// 取得隔天 00:00:00 UTC
fn start_of_next_day(dt: DateTime<Utc>) -> DateTime<Utc> {
    let next_date = dt.date_naive().succ_opt().unwrap_or(dt.date_naive());
    Utc.with_ymd_and_hms(next_date.year(), next_date.month(), next_date.day(), 0, 0, 0).unwrap()
}

/// 將微美元轉為美元（微美元＝10^-6 USD）
pub fn microusd_to_usd(microusd: i64) -> f64 {
    microusd as f64 / 1_000_000.0
}

/// 解析 cost_report 的 amount（美分字串，例如 "23.5018" ＝ $0.235018）
pub fn cents_str_to_usd(cents_str: &str) -> Result<f64> {
    let cents: f64 = cents_str.trim().parse()
        .map_err(|e| anyhow!("invalid cost_report amount '{cents_str}': {e}"))?;
    Ok(cents / 100.0)
}

/// 一筆本機 attempts 的花費紀錄
#[derive(Debug, Clone)]
pub struct AttemptSpent {
    pub created_at: String,
    pub microusd: i64,
}

/// 純計算核心：輸入錨點、cost_report 桶資料、本機 attempts，回傳花費與計算明細
pub fn compute_balance_spent(
    anchor_usd: f64,
    anchor_at_dt: DateTime<Utc>,
    report_buckets: &[CostReportBucket],
    local_attempts: &[AttemptSpent],
    now: DateTime<Utc>,
) -> Result<BalanceCalculation> {
    let t_next_day = start_of_next_day(anchor_at_dt);

    // 1. (a) T 當天花費：只取 local attempts 中 created_at >= T 且 created_at < T_next_day
    let mut spent_t_day_usd = 0.0;
    for att in local_attempts {
        if let Ok(ts) = parse_rfc3339(&att.created_at) {
            if ts >= anchor_at_dt && ts < t_next_day {
                spent_t_day_usd += microusd_to_usd(att.microusd);
            }
        }
    }

    // 2. (b) T 隔天起之 cost_report 完整日
    let mut spent_report_usd = 0.0;
    let mut report_through: Option<DateTime<Utc>> = None;

    // 將 buckets 依 starting_at 排序
    let mut sorted_buckets = report_buckets.to_vec();
    sorted_buckets.sort_by(|a, b| a.starting_at.cmp(&b.starting_at));

    for bucket in &sorted_buckets {
        let b_start = parse_rfc3339(&bucket.starting_at)?;
        let b_end = parse_rfc3339(&bucket.ending_at)?;

        // 只取 >= T 隔天的 bucket，且必須是完整日（結束時間 <= 今天 00:00:00 UTC）
        if b_start >= t_next_day && b_end <= start_of_day(now) {
            let mut bucket_usd = 0.0;
            for res in &bucket.results {
                if let Some(ref amt) = res.amount {
                    bucket_usd += cents_str_to_usd(amt)?;
                }
            }
            spent_report_usd += bucket_usd;
            report_through = Some(report_through.map_or(b_end, |prev| prev.max(b_end)));
        }
    }

    // 3. (c) report 還沒涵蓋的日子（通常是 report_through 起至現在）
    // 如果 report_through 有值，則 local_since = report_through
    // 若 report_through 無值，則 local_since = t_next_day
    let local_since = report_through.unwrap_or(t_next_day);

    let mut spent_recent_local_usd = 0.0;
    for att in local_attempts {
        if let Ok(ts) = parse_rfc3339(&att.created_at) {
            if ts >= local_since {
                spent_recent_local_usd += microusd_to_usd(att.microusd);
            }
        }
    }

    let total_spent = spent_t_day_usd + spent_report_usd + spent_recent_local_usd;
    let remaining_usd = anchor_usd - total_spent;

    Ok(BalanceCalculation {
        anchor_usd,
        anchor_at: am_base::db::iso_at(anchor_at_dt),
        spent_usd: total_spent,
        remaining_usd,
        report_through: report_through.map(am_base::db::iso_at),
        local_since: am_base::db::iso_at(local_since),
        breakdown: BalanceBreakdown {
            t_day_local_usd: spent_t_day_usd,
            report_usd: spent_report_usd,
            recent_local_usd: spent_recent_local_usd,
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceBreakdown {
    pub t_day_local_usd: f64,
    pub report_usd: f64,
    pub recent_local_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceCalculation {
    pub anchor_usd: f64,
    pub anchor_at: String,
    pub spent_usd: f64,
    pub remaining_usd: f64,
    pub report_through: Option<String>,
    pub local_since: String,
    pub breakdown: BalanceBreakdown,
}

fn url_encode(input: &str) -> String {
    input
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// 從 Anthropic 抓取 cost_report（帶分頁）
pub async fn fetch_cost_report(
    admin_key: &str,
    starting_at: &str,
) -> Result<Vec<CostReportBucket>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let mut buckets = Vec::new();
    let mut page: Option<String> = None;

    loop {
        let mut url = format!(
            "https://api.anthropic.com/v1/organizations/cost_report?starting_at={}&bucket_width=1d",
            url_encode(starting_at)
        );
        if let Some(ref p) = page {
            url.push_str("&page=");
            url.push_str(&url_encode(p));
        }

        let resp = client
            .get(&url)
            .header("x-api-key", admin_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic cost_report returned {status}: {body}"));
        }

        let report: CostReportResponse = resp.json().await?;
        buckets.extend(report.data);

        if report.has_more == Some(true) && report.next_page.is_some() {
            page = report.next_page;
        } else {
            break;
        }
    }

    Ok(buckets)
}

/// 查詢 DB 中 created_at >= anchor_at 的 recorded 與 unknown 帳
pub async fn query_local_attempts_since(app: &Arc<App>, since_rfc3339: &str) -> Result<Vec<AttemptSpent>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT created_at,
                COALESCE(CASE WHEN state='recorded' THEN estimated_microusd
                              WHEN state='unknown' THEN COALESCE(estimated_microusd, reserved_microusd)
                              ELSE 0 END, 0)
         FROM api_bot_attempts
         WHERE created_at >= ? AND state IN ('recorded', 'unknown')
         ORDER BY created_at ASC"
    )
    .bind(since_rfc3339)
    .fetch_all(&app.db)
    .await?;

    Ok(rows.into_iter().map(|(created_at, microusd)| AttemptSpent { created_at, microusd }).collect())
}

/// GET /api/claude-api/balance 處理函式
pub async fn get_claude_api_balance(
    app: Arc<App>,
    cache: Arc<BalanceCache>,
) -> Result<Value, LcError> {
    let cfg = app.cfg.get().await;
    let anchor_usd = cfg.claude_api.balance_anchor_usd;
    let anchor_at_str = cfg.claude_api.balance_anchor_at.clone();
    let admin_key_path = cfg.claude_api.admin_key_file.clone()
        .unwrap_or_else(|| "~/.config/agents-manager/anthropic-admin-key".to_string());

    let now = Utc::now();
    let as_of = am_base::db::iso_at(now);

    // 檢查是否有設定錨點與 key
    let (anchor_usd, anchor_at_str) = match (anchor_usd, anchor_at_str) {
        (Some(usd), Some(at)) => (usd, at),
        _ => {
            return Ok(json!({
                "configured": false,
                "as_of": as_of,
            }));
        }
    };

    let anchor_dt = match parse_rfc3339(&anchor_at_str) {
        Ok(dt) => dt,
        Err(e) => {
            return Ok(json!({
                "configured": false,
                "error": format!("invalid balance_anchor_at: {e}"),
                "as_of": as_of,
            }));
        }
    };

    // 讀取 admin key
    let admin_key = match am_base::key_file::read_key(&admin_key_path) {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::warn!("claude-api admin key unreadable: {e}");
            None
        }
    };

    // 抓取本機帳（從 T 的 00:00:00 UTC 開始）
    let t_day_start = am_base::db::iso_at(start_of_day(anchor_dt));
    let local_attempts = match query_local_attempts_since(&app, &t_day_start).await {
        Ok(atts) => atts,
        Err(e) => {
            return Err(LcError::Upstream(format!("failed to query local attempts: {e}")));
        }
    };

    // 嘗試抓取 cost_report
    let mut api_error: Option<String> = None;
    let report_buckets = if let Some(ref key) = admin_key {
        let starting_at_req = am_base::db::iso_at(start_of_next_day(anchor_dt));
        if let Some(cached) = cache.get(&starting_at_req).await {
            cached
        } else {
            match fetch_cost_report(key, &starting_at_req).await {
                Ok(buckets) => {
                    cache.set(starting_at_req, buckets.clone()).await;
                    buckets
                }
                Err(e) => {
                    tracing::warn!("fetch_cost_report failed: {e}");
                    api_error = Some(e.to_string());
                    Vec::new()
                }
            }
        }
    } else {
        api_error = Some("admin key unavailable".into());
        Vec::new()
    };

    // 計算
    let calc = compute_balance_spent(
        anchor_usd,
        anchor_dt,
        &report_buckets,
        &local_attempts,
        now,
    ).map_err(|e| LcError::Bad(format!("calculation error: {e}")))?;

    let mut resp = json!({
        "configured": true,
        "anchor": {
            "usd": calc.anchor_usd,
            "at": calc.anchor_at,
        },
        "spent_usd": calc.spent_usd,
        "remaining_usd": calc.remaining_usd,
        "report_through": calc.report_through,
        "local_since": calc.local_since,
        "breakdown": calc.breakdown,
        "as_of": as_of,
    });

    if let Some(err) = api_error {
        resp["error"] = json!(err);
    }

    Ok(resp)
}

/// PUT /api/claude-api/balance-anchor 請求 body
#[derive(Debug, Deserialize)]
pub struct PutBalanceAnchorRequest {
    pub usd: f64,
    pub at: Option<String>,
}

pub async fn put_claude_api_balance_anchor(
    app: Arc<App>,
    cache: Arc<BalanceCache>,
    body: PutBalanceAnchorRequest,
) -> Result<Value, LcError> {
    if body.usd < 0.0 || !body.usd.is_finite() {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_usd"})));
    }

    let at_str = match body.at {
        Some(ref at) => {
            parse_rfc3339(at).map_err(|e| LcError::BadValue(json!({"error": "bad_request", "reason": e.to_string()})))?;
            at.clone()
        }
        None => am_base::db::now(),
    };

    let usd_val = body.usd;
    let at_val = at_str.clone();

    app.cfg.update(move |cfg| {
        cfg.claude_api.balance_anchor_usd = Some(usd_val);
        cfg.claude_api.balance_anchor_at = Some(at_val);
        Ok(())
    }).await.map_err(crate::api::cfg_err)?;

    cache.invalidate().await;

    Ok(json!({
        "ok": true,
        "anchor": {
            "usd": usd_val,
            "at": at_str,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cents_to_usd() {
        assert_eq!(cents_str_to_usd("23.5018").unwrap(), 0.235018);
        assert_eq!(cents_str_to_usd("0").unwrap(), 0.0);
        assert_eq!(cents_str_to_usd("100").unwrap(), 1.0);
        assert!(cents_str_to_usd("abc").is_err());
    }

    #[test]
    fn test_microusd_to_usd() {
        assert_eq!(microusd_to_usd(1_000_000), 1.0);
        assert_eq!(microusd_to_usd(500_000), 0.5);
        assert_eq!(microusd_to_usd(41_808), 0.041808);
    }

    #[test]
    fn test_compute_balance_spent_all_three_tiers() {
        // T 錨點：2026-11-01T12:00:00Z，餘額 B = 100.0 USD
        let anchor_at = DateTime::parse_from_rfc3339("2026-11-01T12:00:00Z").unwrap().with_timezone(&Utc);
        let anchor_usd = 100.0;

        // 當前時間：2026-11-04T15:00:00Z
        let now = DateTime::parse_from_rfc3339("2026-11-04T15:00:00Z").unwrap().with_timezone(&Utc);

        // 本機 attempts：
        // 1. T 之前：2026-11-01T10:00:00Z (不計入)
        // 2. T 當天：2026-11-01T14:00:00Z (計入 tier a，100,000 µUSD = $0.10)
        // 3. 2026-11-02T10:00:00Z (此日有 report，不應從 local 計入)
        // 4. 2026-11-04T08:00:00Z (今天尚未有 report，計入 tier c，200,000 µUSD = $0.20)
        let local_attempts = vec![
            AttemptSpent { created_at: "2026-11-01T10:00:00Z".into(), microusd: 500_000 },
            AttemptSpent { created_at: "2026-11-01T14:00:00Z".into(), microusd: 100_000 },
            AttemptSpent { created_at: "2026-11-02T10:00:00Z".into(), microusd: 300_000 },
            AttemptSpent { created_at: "2026-11-04T08:00:00Z".into(), microusd: 200_000 },
        ];

        // cost_report：
        // 涵蓋 2026-11-02 (23.5018 cents = $0.235018)
        // 涵蓋 2026-11-03 (100.0000 cents = $1.000000)
        let report_buckets = vec![
            CostReportBucket {
                starting_at: "2026-11-02T00:00:00Z".into(),
                ending_at: "2026-11-03T00:00:00Z".into(),
                results: vec![CostReportResult { currency: Some("USD".into()), amount: Some("23.5018".into()) }],
            },
            CostReportBucket {
                starting_at: "2026-11-03T00:00:00Z".into(),
                ending_at: "2026-11-04T00:00:00Z".into(),
                results: vec![CostReportResult { currency: Some("USD".into()), amount: Some("100.0000".into()) }],
            },
        ];

        let res = compute_balance_spent(anchor_usd, anchor_at, &report_buckets, &local_attempts, now).unwrap();

        // (a) T 當天：$0.10
        assert!((res.breakdown.t_day_local_usd - 0.10).abs() < 1e-6);
        // (b) report：$0.235018 + $1.00 = $1.235018
        assert!((res.breakdown.report_usd - 1.235018).abs() < 1e-6);
        // (c) 未涵蓋日 (2026-11-04)：$0.20
        assert!((res.breakdown.recent_local_usd - 0.20).abs() < 1e-6);

        // 總花費：0.10 + 1.235018 + 0.20 = 1.535018
        let expected_spent = 0.10 + 1.235018 + 0.20;
        assert!((res.spent_usd - expected_spent).abs() < 1e-6);
        assert!((res.remaining_usd - (100.0 - expected_spent)).abs() < 1e-6);

        assert_eq!(res.report_through, Some("2026-11-04T00:00:00.000Z".into()));
        assert_eq!(res.local_since, "2026-11-04T00:00:00.000Z");
    }

    #[test]
    fn test_compute_balance_fallback_no_report() {
        // 沒有 report 資料時，T 隔天起全部走本機帳
        let anchor_at = DateTime::parse_from_rfc3339("2026-11-01T12:00:00Z").unwrap().with_timezone(&Utc);
        let anchor_usd = 50.0;
        let now = DateTime::parse_from_rfc3339("2026-11-02T15:00:00Z").unwrap().with_timezone(&Utc);

        let local_attempts = vec![
            AttemptSpent { created_at: "2026-11-01T14:00:00Z".into(), microusd: 100_000 }, // $0.10
            AttemptSpent { created_at: "2026-11-02T05:00:00Z".into(), microusd: 400_000 }, // $0.40
        ];

        let res = compute_balance_spent(anchor_usd, anchor_at, &[], &local_attempts, now).unwrap();
        assert!((res.breakdown.t_day_local_usd - 0.10).abs() < 1e-6);
        assert_eq!(res.breakdown.report_usd, 0.0);
        assert!((res.breakdown.recent_local_usd - 0.40).abs() < 1e-6);
        assert_eq!(res.report_through, None);
        assert_eq!(res.local_since, "2026-11-02T00:00:00.000Z");
        assert!((res.spent_usd - 0.50).abs() < 1e-6);
        assert!((res.remaining_usd - 49.50).abs() < 1e-6);
    }
}
