//! codex 額度重置券的「使用」：只在使用者按下按鈕時呼叫 codex app-server 的
//! `account/rateLimitResetCredit/consume`（schema 見 `codex app-server generate-json-schema`，
//! `ConsumeAccountRateLimitResetCreditParams { creditId?, idempotencyKey }`）。
//! 花券是使用者的決定，所以這條路徑只給 User 呼叫（api.rs 的 `BOT_ROUTE_POLICIES` 擋 bot／AGM）。

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::db;
use crate::quota::{quota_base_for_host, quota_key, ResetCredit, ResetCredits};
use crate::state::App;

/// codex app-server 的兌換方法名（2026-11-08 查 `generate-json-schema` 確認）。
pub const CONSUME_METHOD: &str = "account/rateLimitResetCredit/consume";

/// 撞限停工的 codex bot 收到的續作提示（走正常 prompt 佇列）。
pub const RESUME_TEXT: &str = "額度已重置，請繼續剛才的工作。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Reset => "reset",
            Outcome::NothingToReset => "nothingToReset",
            Outcome::NoCredit => "noCredit",
            Outcome::AlreadyRedeemed => "alreadyRedeemed",
        }
    }
}

#[derive(Debug)]
pub enum RedeemError {
    /// 這個帳號目前沒有可用的券（daemon 讀到的 `available` 是 0，或帳號沒有額度讀數）。不呼叫 app-server。
    NoCredit,
    /// 指定的 `credit_id` 不在這個帳號可用的券裡。
    UnknownCredit,
    /// app-server 呼叫失敗或回了不認得的 outcome。
    Rpc(anyhow::Error),
}

pub struct Redeemed {
    pub outcome: Outcome,
    pub key: String,
    /// 這次重置後收到續作提示的 bot id。
    pub resumed: Vec<String>,
}

pub fn parse_outcome(v: &Value) -> Result<Outcome> {
    match v.get("outcome").and_then(Value::as_str) {
        Some("reset") => Ok(Outcome::Reset),
        Some("nothingToReset") => Ok(Outcome::NothingToReset),
        Some("noCredit") => Ok(Outcome::NoCredit),
        Some("alreadyRedeemed") => Ok(Outcome::AlreadyRedeemed),
        other => Err(anyhow!("unexpected reset-credit outcome {other:?}: {v}")),
    }
}

/// 指定 id 就必須是可用的那張；沒指定就挑最早到期的可用券（沒到期日排最後）。
pub fn pick_credit<'a>(rc: &'a ResetCredits, wanted: Option<&str>) -> Option<&'a ResetCredit> {
    let available = rc.credits.iter().filter(|c| c.status == "available");
    match wanted {
        Some(id) => available.into_iter().find(|c| c.id == id),
        None => available.min_by_key(|c| {
            c.expires_at
                .as_deref()
                .and_then(crate::quota::parse_utc)
                .map_or(i64::MAX, |t| t.timestamp())
        }),
    }
}

/// 同一次嘗試用同一把 key（app-server 以此去重）。128 位隨機值，格式同 UUID 的 hex 形狀即可。
fn new_idempotency_key() -> String {
    format!("{:032x}", rand::random::<u128>())
}

pub async fn redeem(app: &Arc<App>, host: &str, identity: Option<&str>, credit_id: Option<&str>) -> Result<Redeemed, RedeemError> {
    let base = quota_base_for_host(&**app, host, "codex", identity).await;
    let key = quota_key(host, &base);
    let current = app.quotas.lock().await.get(&key).cloned();
    let rc = current
        .and_then(|q| q.reset_credits)
        .filter(|rc| rc.available > 0)
        .ok_or(RedeemError::NoCredit)?;
    // 清單是空的（舊讀數沒有 `credits`）就不指定 id，交給 app-server 挑下一張。
    let credit = if rc.credits.is_empty() {
        None
    } else {
        Some(pick_credit(&rc, credit_id).ok_or(RedeemError::UnknownCredit)?)
    };
    let wire_id = credit.map(|c| c.id.clone());

    let home = match identity {
        Some(name) => crate::models::identity_home_dir(app, host, Some(name), "codex", "CODEX_HOME")
            .await
            .map_err(RedeemError::Rpc)?,
        None => None,
    };
    let params = json!({ "idempotencyKey": new_idempotency_key(), "creditId": wire_id });
    let r = crate::runners::models::codex_rpc_in(app, host, home.as_deref(), CONSUME_METHOD, params)
        .await
        .map_err(RedeemError::Rpc)?;
    let outcome = parse_outcome(&r).map_err(RedeemError::Rpc)?;

    // 先刷新額度再通知：bot 醒來看到的就是新的讀數。刷新失敗不影響兌換結果（下一輪輪詢會補）。
    if let Err(e) = crate::runners::quota::refresh_codex(app, host).await {
        tracing::warn!(host, error = %e, "reset credit used; quota refresh failed");
    }
    let resumed = if outcome == Outcome::Reset {
        resume_held_bots(app, host, &base).await
    } else {
        Vec::new()
    };
    Ok(Redeemed { outcome, key, resumed })
}

/// 停工中的 codex bot（額度擋下了它排著的 prompt）。**持久化**：擋下時寫在排著那一列的 `turns.quota_hold`（DB），
/// daemon 重啟後仍讀得到；行程內剛被閘門擋下、還沒寫進 DB 的（`recently_held`）也算。
pub async fn held_codex_bots(app: &Arc<App>) -> Vec<db::Bot> {
    let held_ids: Vec<String> = match sqlx::query_scalar(
        "SELECT DISTINCT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued' AND t.quota_hold IS NOT NULL",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "reset credit: cannot read the persisted quota holds");
            Vec::new()
        }
    };
    let bots: Vec<db::Bot> = sqlx::query_as("SELECT * FROM bots WHERE kind = 'codex' AND deleted_at IS NULL")
        .fetch_all(&app.db)
        .await
        .unwrap_or_default();
    bots.into_iter()
        .filter(|b| held_ids.contains(&b.id) || crate::lifecycle::quota_hold::recently_held(&b.id))
        .collect()
}

/// 額度列（`/api/quota` 回的 `kinds` map）上每個 codex 格補 `held_bots`：停工那幾顆的名稱。
/// 網頁的重置券提示靠它列出受影響的 bot。
pub async fn annotate_held_bots(app: &Arc<App>, snap: &mut Value) {
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for bot in held_codex_bots(app).await {
        let Ok(host) = db::bot_host(&app.db, &bot.id).await else { continue };
        let base = quota_base_for_host(&**app, &host, "codex", bot.identity.as_deref()).await;
        by_key.entry(quota_key(&host, &base)).or_default().push(bot.name);
    }
    let Some(kinds) = snap.get_mut("kinds").and_then(Value::as_object_mut) else { return };
    for (key, names) in by_key {
        if let Some(cell) = kinds.get_mut(&key).and_then(Value::as_object_mut) {
            cell.insert("held_bots".into(), json!(names));
        }
    }
}

/// 這台、這個帳號上、因為撞限被擋住的 codex bot 收一則續作提示（同一個 prompt 佇列，不另開通道）。
async fn resume_held_bots(app: &Arc<App>, host: &str, base: &str) -> Vec<String> {
    let mut resumed = Vec::new();
    for bot in held_codex_bots(app).await {
        if db::bot_host(&app.db, &bot.id).await.ok().as_deref() != Some(host) {
            continue;
        }
        if quota_base_for_host(&**app, host, "codex", bot.identity.as_deref()).await != base {
            continue;
        }
        let client_request_id = format!("reset-credit-{}", new_idempotency_key());
        match crate::lifecycle::prompt_relayed(app, &bot.id, RESUME_TEXT, &client_request_id, &[], Some(crate::agent_relay::DAEMON_SENDER)).await {
            Ok(_) => resumed.push(bot.id),
            Err(e) => tracing::warn!(bot = %bot.id, error = ?e, "reset credit: resume prompt not queued"),
        }
    }
    resumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::Quota;

    fn snapshot(rc: ResetCredits) -> Quota {
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: Some(rc),
            limit_hit: None,
            plan: None,
            updated_at: String::new(),
            source: "codex-app-server".into(),
            account: None,
            host: crate::config::LOCAL_HOST.into(),
        }
    }

    fn credit(id: &str, status: &str, expires: Option<&str>) -> ResetCredit {
        ResetCredit { id: id.into(), title: Some("Full reset".into()), status: status.into(), granted_at: None, expires_at: expires.map(String::from) }
    }

    #[test]
    fn outcomes_are_read_by_name_and_unknown_ones_are_errors() {
        assert_eq!(parse_outcome(&json!({"outcome": "reset"})).unwrap(), Outcome::Reset);
        assert_eq!(parse_outcome(&json!({"outcome": "nothingToReset"})).unwrap(), Outcome::NothingToReset);
        assert_eq!(parse_outcome(&json!({"outcome": "noCredit"})).unwrap(), Outcome::NoCredit);
        assert_eq!(parse_outcome(&json!({"outcome": "alreadyRedeemed"})).unwrap(), Outcome::AlreadyRedeemed);
        assert!(parse_outcome(&json!({"outcome": "bogus"})).is_err());
        assert!(parse_outcome(&json!({})).is_err());
    }

    #[test]
    fn no_id_picks_the_soonest_available_credit_and_never_a_spent_one() {
        let rc = ResetCredits {
            available: 2,
            title: None,
            expires_at: None,
            credits: vec![
                credit("late", "available", Some("2026-11-30T00:00:00Z")),
                credit("spent", "redeemed", Some("2026-11-01T00:00:00Z")),
                credit("soon", "available", Some("2026-11-15T00:00:00Z")),
                credit("undated", "available", None),
            ],
        };
        assert_eq!(pick_credit(&rc, None).unwrap().id, "soon");
        assert_eq!(pick_credit(&rc, Some("late")).unwrap().id, "late");
        assert!(pick_credit(&rc, Some("spent")).is_none(), "已兌換的券不能再指定");
        assert!(pick_credit(&rc, Some("nope")).is_none());
    }

    #[test]
    fn idempotency_keys_differ_between_attempts() {
        assert_ne!(new_idempotency_key(), new_idempotency_key());
        assert_eq!(new_idempotency_key().len(), 32);
    }

    /// 停工名單寫在 DB：不靠行程內的記憶體也認得出來（重啟後的 `recently_held` 是空的）；`held_bots` 補在額度格上。
    #[tokio::test]
    async fn held_codex_bots_come_from_the_database_and_are_named_on_the_quota_cell() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'codex-held','codex','[]',0,1,'tok','user',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
            .bind(format!("conv-{bot_id}"))
            .bind(&bot_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, quota_hold)
             VALUES (?,?,'web','queued','pending','派工',?,'{}')",
        )
        .bind(format!("turn-{bot_id}"))
        .bind(format!("conv-{bot_id}"))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert!(!crate::lifecycle::quota_hold::recently_held(&bot_id), "行程內沒有記憶體，只能靠 DB");
        assert!(held_codex_bots(&app).await.iter().any(|b| b.id == bot_id));

        let mut snap = json!({"kinds": {"codex": {"five_hour": null}, "claude": {"five_hour": null}}});
        annotate_held_bots(&app, &mut snap).await;
        assert_eq!(snap["kinds"]["codex"]["held_bots"], json!(["codex-held"]), "{snap}");
        assert!(snap["kinds"]["claude"].get("held_bots").is_none());

        // 放行（清掉 DB 的憑據）之後就不再算停工。
        sqlx::query("UPDATE turns SET quota_hold = NULL WHERE id = ?").bind(format!("turn-{bot_id}")).execute(&app.db).await.unwrap();
        assert!(!held_codex_bots(&app).await.iter().any(|b| b.id == bot_id));
    }

    /// 沒有券（或沒有讀數）就不碰 app-server：直接 NoCredit。這是「有券才提示、沒券不能按」的後端那一半。
    #[tokio::test]
    async fn redeem_without_a_credit_never_calls_codex() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        assert!(matches!(redeem(&app, crate::config::LOCAL_HOST, None, None).await, Err(RedeemError::NoCredit)));
        app.quotas.lock().await.insert("codex".into(), snapshot(ResetCredits { available: 0, title: None, expires_at: None, credits: vec![] }));
        assert!(matches!(redeem(&app, crate::config::LOCAL_HOST, None, None).await, Err(RedeemError::NoCredit)));
    }

    #[tokio::test]
    async fn redeem_with_an_unknown_credit_id_is_refused_before_codex() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let rc = ResetCredits { available: 1, title: None, expires_at: None, credits: vec![credit("real", "available", None)] };
        app.quotas.lock().await.insert("codex".into(), snapshot(rc));
        assert!(matches!(
            redeem(&app, crate::config::LOCAL_HOST, None, Some("fake")).await,
            Err(RedeemError::UnknownCredit)
        ));
    }
}
