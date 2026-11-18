//! 協調者倒掉時，核准要自己換人裁示（issue #421）。
//!
//! 以前的形狀：`approval_requested` 只送給協調者（`roles::route`），協調者沒登入就一直送不進去——
//! notify 補送 60～80 次、`gave_up`、核准 90 分鐘過期、kick 下個整點重新申請，一輪一小時。
//! 2026-09-23 晚上因此停了 9 小時，而巡檢（使用者入口）整晚是綠燈：它「沒有事情要做」，
//! 因為那件事掛在另一個角色底下。
//!
//! 現在：核准**開著超過 5 分鐘**而協調者不可用，就把那一則改路由給巡檢並叫醒它。巡檢用同一套
//! `agm approval decide` 裁示；協調者恢復之後**不搶回**已經改派的事件（`roles::classify` 只補
//! `role IS NULL` 的列，所以寫過 `patrol` 就定了）。再開著超過 30 分鐘沒有任何人裁示 → incident
//! `approval_stalled`，走 incidents 那條路喊人。
//!
//! 只有 `approval_requested` 會改派。`bot_request`、`mission_*` 照舊留在協調者的佇列等它回來
//! （SPEC §18.15）：那些是「請協調者處理一件事」，換人做沒有意義；核准不一樣，它是一道閘門，
//! 卡住的不是協調者的工作而是別人的部署。
//!
//! **沒有證據就不動**：`health::RoleState::Unknown`（讀不到畫面、DB 錯、liveness 讀不到）一律當成
//! 「這一拍不判斷」。把核准從一顆其實健康的協調者手上搬走，比晚 5 分鐘糟得多——兩個角色都以為
//! 對方在處理，就沒有人在處理。


use serde_json::{json, Value};

use sqlx::SqlitePool;

use super::roles::{self, Role};
use super::store;

/// 核准開著超過這麼久、協調者又不可用，就改派給巡檢。
///
/// 5 分鐘是使用者定的（2026-09-24）：夠久到「協調者只是正忙著上一個回合」不會被誤判成倒了
/// （notify 的批次窗是 `responder_batch_secs`，預設遠小於這個數），又短到一個晚上不會白等。
pub const REASSIGN_AFTER_SECS: i64 = 300;

/// 開著超過這麼久、**不論在誰手上**都還沒有裁示 → incident。改派本身也可能沒用（巡檢也沒登入），
/// 那時要喊的是人，不是再換一個角色。
pub const STALLED_AFTER_SECS: i64 = 1800;

/// incident 的 kind。resource 用 approval id，所以一筆核准只會開一個。
pub const STALLED_KIND: &str = "approval_stalled";

/// 會改派的事件種類。刻意只有一種——見模組註解。
const REASSIGNABLE: &str = "approval_requested";

/// 協調者現在可不可用。`None` 同時代表「可用」與「**不知道**」——兩者都不改派。
///
/// 判定本身在 `health::responder_state`（#420 後續，sha 770201d8）：DB 的 `status`
/// （`needs_login`／`waiting_quota`）→ `active_run`（`no_run`）→ `Available`，讀不到就 `Unknown`。
/// #427 會在中間再插「讀畫面的結論」與 notify 連續失敗，多出來的只會讓改派**更早**觸發。
///
/// 這裡用 `is_unavailable()` 當閘門（而不是自己 `match`）：`Unknown` 與 `NotConfigured` 都不算不可用，
/// 照 reason 自己 match 很容易手滑把 `Unknown` 算進去。把核准從一顆其實健康的協調者手上搬走，
/// 比晚 5 分鐘糟得多——兩個角色都以為對方在處理，就沒有人在處理。
/// `reason()` 只用來當寫進 payload 的標籤，字串是對外契約（`health::REASON_*`）。
pub async fn responder_unavailable(app: &(impl crate::capabilities::Db + crate::supervisor::role_faults::RoleFaultTable)) -> Option<&'static str> {
    let state = super::health::responder_state(app).await;
    if !state.is_unavailable() {
        return None;
    }
    state.reason()
}

/// 這一則該不該改派。**門檻只寫在這裡**，SQL 不做時間算術。
///
/// 以前 SQL 自己算 `created_at <= iso_in(-REASSIGN_AFTER_SECS)`，而這支函式只有它自己的測試在呼叫
/// （i407 review 2026-09-24）：299／300 秒的邊界斷言測的是沒人跑的程式，把 SQL 的正負號弄反
/// （變成「未來 5 分鐘」、一則都撈不到）照樣全綠。現在查詢只負責縮小範圍（開著的、在協調者手上的核准，
/// 數量本來就是一兩則），要不要動由這支決定——門檻一份，而且走得到。
///
/// `unavailable` 是 `Some(reason)` 才動：`None` 同時代表「可用」與「不知道」，兩種都不該改派。
pub fn should_reassign(age_secs: i64, owner: &str, unavailable: Option<&str>) -> bool {
    unavailable.is_some() && owner == Role::Responder.as_str() && age_secs >= REASSIGN_AFTER_SECS
}

/// 把協調者手上等太久的核准改派給巡檢，並在協調者可用或不可用時都收掉已經不用裁示的核准（issue #881、#1120）。
/// 回傳改派了幾則。
///
/// `unavailable` = `health::responder_state` 回 `Unavailable(reason)` 時的那個原因字串；
/// 可用、不知道、沒建立協調者都傳 `None`（此時不改派，但照舊收掉過期或已有結果的核准）。
pub async fn reassign_stale_approvals(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), unavailable: Option<&'static str>) -> usize {
    let reason = unavailable;
    let now = crate::db::now();
    // 查詢只縮小範圍，不判門檻（見 `should_reassign`）：開著的核准一次本來就一兩則。
    let open = match sqlx::query_as::<_, store::InboxEvent>(
        "SELECT * FROM supervisor_inbox
          WHERE supervisor_id=? AND kind=? AND state!='handled'
          ORDER BY created_at ASC, rowid ASC",
    )
    .bind(store::SUPERVISOR_ID)
    .bind(REASSIGNABLE)
    .fetch_all(app.db())
    .await
    {
        Ok(v) => v,
        // 讀不到就不動。下一拍再看。
        Err(e) => {
            tracing::warn!(error = %e, "could not look for approvals to reassign");
            return 0;
        }
    };
    let mut moved = 0usize;
    for e in &open {
        // 那筆核准已經不用裁示了（被裁示、取代、過期）：改派過去只會叫醒巡檢去處理一筆作廢的申請。直接收掉這則（#881、#1120）。
        match approval_needs_no_decision(app.db(), e, &now).await {
            Ok(true) => {
                match sqlx::query("UPDATE supervisor_inbox SET state='handled', acked_by='daemon', updated_at=? WHERE id=? AND state!='handled'")
                    .bind(&now)
                    .bind(&e.id)
                    .execute(app.db())
                    .await
                {
                    Ok(_) => tracing::info!(event = %e.id, "approval no longer pending or already expired; closed its inbox event instead of reassigning"),
                    Err(err) => tracing::warn!(event = %e.id, error = %err, "could not close the inbox event of a settled approval"),
                }
                continue;
            }
            Ok(false) => {}
            // 讀不到就不動這一則，下一拍再看。
            Err(err) => {
                tracing::warn!(event = %e.id, error = %err, "could not read the approval behind an inbox event");
                continue;
            }
        }
        let Some(reason) = reason else { continue };
        // 門檻在這裡判，不在 SQL。`owner` 走 `roles::owner_or_default`，跟查詢的 `roles::OWNER` 同一個定義。
        let owner = roles::owner_or_default(e.claimed_by.as_deref(), e.role.as_deref());
        if !should_reassign(age_secs(&e.created_at), &owner, Some(reason)) {
            continue;
        }
        match reassign_one(app.db(), e, reason, &now).await {
            Ok(true) => {
                moved += 1;
                tracing::warn!(event = %e.id, reason, "approval reassigned to patrol: the responder is unavailable");
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(event = %e.id, error = %err, "could not reassign an approval"),
        }
    }
    if moved > 0 {
        if let Some(reason) = reason {
            app.emit("supervisor_changed", json!({"approvals_reassigned": moved, "to_role": Role::Patrol.as_str(), "reason": reason})).await;
        }
    }
    moved
}

/// 這則 `approval_requested` 指向的核准是否已經不需要裁示：不是 `pending` 了，或已過有效期。
/// payload 是 `Approval::to_json()`（鍵 `id`）；找不到那筆核准（沒有 id、查無列）就當作不知道，照舊改派。
async fn approval_needs_no_decision(db: &SqlitePool, e: &store::InboxEvent, now: &str) -> anyhow::Result<bool> {
    let payload: Value = serde_json::from_str(&e.payload_json).unwrap_or_else(|_| json!({}));
    let Some(id) = payload.get("approval_id").or_else(|| payload.get("id")).and_then(Value::as_str) else { return Ok(false) };
    let Some(a) = store::approval(db, id).await? else { return Ok(false) };
    if a.status != "pending" {
        return Ok(true);
    }
    Ok(a.effective_expiry().is_some_and(|t| crate::db::cmp_ts(&t, now).is_le()))
}

/// 一則的改派。條件寫在 SQL 裡（`{owner}` 還是協調者才改），所以兩拍之間巡檢已經收走時不會重複寫。
async fn reassign_one(db: &SqlitePool, e: &store::InboxEvent, reason: &str, now: &str) -> anyhow::Result<bool> {
    let mut payload: Value = serde_json::from_str(&e.payload_json).unwrap_or_else(|_| json!({}));
    if let Some(o) = payload.as_object_mut() {
        o.insert("reassigned_from".into(), json!(Role::Responder.as_str()));
        o.insert("reassigned_reason".into(), json!(reason));
        o.insert("reassigned_at".into(), json!(now));
        // 路由表讀這個欄位，所以重新分類（或 payload 被別的地方重讀）時答案一樣是巡檢。
        o.insert("to_role".into(), json!(Role::Patrol.as_str()));
    }
    // `notify_*` 一起歸零：那些次數是協調者送不出去累積的，留著會吃掉巡檢的補送額度
    // （`roles::due_for` 對巡檢有 `notify_attempts < max` 的上限），等於改派過去就已經用完了。
    // `claimed_by` 清掉、`state` 回 pending：巡檢還沒被告知過這一則。
    let n = sqlx::query(&format!(
        "UPDATE supervisor_inbox
            SET role='patrol', claimed_by=NULL, wake=1, state='pending', payload_json=?,
                notify_attempts=0, notify_next_at=NULL, notify_turn_id=NULL, notify_delivery=NULL,
                notify_error=NULL, delivered_at=NULL, updated_at=?
          WHERE id=? AND {owner}='responder' AND state!='handled'",
        owner = roles::OWNER
    ))
    .bind(payload.to_string())
    .bind(now)
    .bind(&e.id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

type StalledApprovalRow = (String, String, Option<String>, String, Option<String>);

/// 開著超過 30 分鐘、還沒有人裁示的核准。給 `incidents::observe` 用。
///
/// 不看在誰手上：改派之後巡檢也沒登入時，要喊的是人。`wait_since` 是被取代時接過來的等待起點，
/// 有就用它——重申請接續的等待不能因為換了一筆 id 就從零開始算。
pub async fn stalled_approvals(db: &SqlitePool, after_secs: i64) -> anyhow::Result<Vec<(String, i64, String)>> {
    let rows: Vec<StalledApprovalRow> =
        sqlx::query_as("SELECT id, created_at, wait_since, requester, expires_at FROM supervisor_approvals WHERE supervisor_id=? AND status='pending'")
            .bind(store::SUPERVISOR_ID)
            .fetch_all(db)
            .await?;
    let now = crate::db::now();
    let mut out = vec![];
    for (id, created_at, wait_since, requester, expires_at) in rows {
        // 已過有效期的 pending 沒有人需要裁示（核准不能再用）：不算卡住。秒格式舊值靠 `cmp_ts` 正規化，不用字串比。
        if expires_at.as_deref().is_some_and(|t| crate::db::cmp_ts(t, &now).is_le()) {
            continue;
        }
        let since = wait_since.unwrap_or(created_at);
        let age = age_secs(&since);
        if age >= after_secs {
            out.push((id, age, requester));
        }
    }
    Ok(out)
}

fn age_secs(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0)
}

/// 改派之後誰擁有這一則。測試與 SPEC 都讀這個。
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub async fn owner_of(pool: &sqlx::SqlitePool, event_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!("SELECT {owner} FROM supervisor_inbox WHERE id=?", owner = roles::OWNER))
        .bind(event_id)
        .fetch_optional(pool)
        .await?)
}
