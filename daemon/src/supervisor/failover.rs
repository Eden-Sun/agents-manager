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

use std::sync::Arc;

use serde_json::{json, Value};

use crate::state::App;

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
pub async fn responder_unavailable(app: &Arc<App>) -> Option<&'static str> {
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

/// 把協調者手上等太久的核准改派給巡檢。回傳改派了幾則。
///
/// `unavailable` = `health::responder_state` 回 `Unavailable(reason)` 時的那個原因字串；
/// 可用、不知道、沒建立協調者都傳 `None`（那時這個函式什麼都不做）。
pub async fn reassign_stale_approvals(app: &Arc<App>, unavailable: Option<&'static str>) -> usize {
    let Some(reason) = unavailable else { return 0 };
    let now = crate::db::now();
    // 查詢只縮小範圍，不判門檻（見 `should_reassign`）：開著的、還在協調者手上的核准，一次本來就一兩則。
    let open = match sqlx::query_as::<_, store::InboxEvent>(&format!(
        "SELECT * FROM supervisor_inbox
          WHERE supervisor_id=? AND kind=? AND state!='handled' AND {owner}=?
          ORDER BY created_at ASC, rowid ASC",
        owner = "COALESCE(claimed_by, role)"
    ))
    .bind(store::SUPERVISOR_ID)
    .bind(REASSIGNABLE)
    .bind(Role::Responder.as_str())
    .fetch_all(&app.db)
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
        // 門檻在這裡判，不在 SQL。`owner` 用 `COALESCE(claimed_by, role)`，跟查詢同一個定義。
        let owner = e.claimed_by.clone().or_else(|| e.role.clone()).unwrap_or_default();
        if !should_reassign(age_secs(&e.created_at), &owner, Some(reason)) {
            continue;
        }
        match reassign_one(app, e, reason, &now).await {
            Ok(true) => {
                moved += 1;
                tracing::warn!(event = %e.id, reason, "approval reassigned to patrol: the responder is unavailable");
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(event = %e.id, error = %err, "could not reassign an approval"),
        }
    }
    if moved > 0 {
        app.emit("supervisor_changed", json!({"approvals_reassigned": moved, "to_role": Role::Patrol.as_str(), "reason": reason})).await;
    }
    moved
}

/// 一則的改派。條件寫在 SQL 裡（`{owner}` 還是協調者才改），所以兩拍之間巡檢已經收走時不會重複寫。
async fn reassign_one(app: &Arc<App>, e: &store::InboxEvent, reason: &str, now: &str) -> anyhow::Result<bool> {
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
    let n = sqlx::query(
        "UPDATE supervisor_inbox
            SET role='patrol', claimed_by=NULL, wake=1, state='pending', payload_json=?,
                notify_attempts=0, notify_next_at=NULL, notify_turn_id=NULL, notify_delivery=NULL,
                notify_error=NULL, delivered_at=NULL, updated_at=?
          WHERE id=? AND COALESCE(claimed_by, role)='responder' AND state!='handled'",
    )
    .bind(payload.to_string())
    .bind(now)
    .bind(&e.id)
    .execute(&app.db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// 開著超過 30 分鐘、還沒有人裁示的核准。給 `incidents::observe` 用。
///
/// 不看在誰手上：改派之後巡檢也沒登入時，要喊的是人。`wait_since` 是被取代時接過來的等待起點，
/// 有就用它——重申請接續的等待不能因為換了一筆 id 就從零開始算。
pub async fn stalled_approvals(app: &Arc<App>, after_secs: i64) -> anyhow::Result<Vec<(String, i64, String)>> {
    let rows: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, created_at, wait_since, requester FROM supervisor_approvals WHERE supervisor_id=? AND status='pending'")
            .bind(store::SUPERVISOR_ID)
            .fetch_all(&app.db)
            .await?;
    let mut out = vec![];
    for (id, created_at, wait_since, requester) in rows {
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
pub async fn owner_of(pool: &sqlx::SqlitePool, event_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT COALESCE(claimed_by, role) FROM supervisor_inbox WHERE id=?").bind(event_id).fetch_optional(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `should_reassign` 的組合表。它**就是**生產路徑用的那一份判斷（`reassign_stale_approvals`
    /// 逐則呼叫它），所以這裡的邊界斷言不再是死碼；真實路徑另有
    /// `the_five_minute_line_is_exact_on_the_path_that_actually_runs` 從 SQL 進來再走一次。
    #[test]
    fn only_an_unavailable_responder_and_a_five_minute_wait_move_an_approval() {
        assert!(should_reassign(300, "responder", Some("needs_login")), "滿 5 分鐘＋不可用 → 改派");
        assert!(should_reassign(9000, "responder", Some("waiting_quota")), "撞限也算不可用（票上第 1 點）");
        assert!(!should_reassign(299, "responder", Some("needs_login")), "還沒滿 5 分鐘不動");
        // `None` 同時是「可用」與「不知道」：兩種都不該改派。
        assert!(!should_reassign(9000, "responder", None), "可用／讀不到都不動");
        assert!(!should_reassign(9000, "patrol", Some("needs_login")), "已經在巡檢手上的不再動");
    }

    /// 我依賴 `health::responder_state`（#420 後續）的哪些格子。這不是重測它的實作，而是釘住
    /// **改派要靠的契約**：三個 reason 字串、沒建立就不動、以及「讀不到不算不可用」。
    /// 它哪天多回一種 Unavailable（#427 的 notify_stalled）不會讓這條紅——多的只會更早改派。
    #[tokio::test]
    async fn the_contract_reassignment_depends_on_is_the_one_role_state_publishes() {
        let app = app().await;
        // 還沒建立協調者：沒有東西要改派。
        assert_eq!(responder_unavailable(&app).await, None, "沒建立協調者");

        // 登記一顆 bot（還沒有 run）。
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(crate::db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('resp','p','AGM-responder','claude','tok',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        roles::set_env(&app.db, Role::Responder, "resp", "p", "/tmp").await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "登記了卻沒有 run");

        // 有 run：可用。
        sqlx::query(
            "INSERT INTO runs (id,bot_id,state,agent_status,workspace_id,pane_id,agent_name,herdr_session,pane_typed,started_at)
             VALUES ('r1','resp','running','idle','ws','pane-r','AGM-responder','test',1,?)",
        )
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(responder_unavailable(&app).await, None, "有 run 就當可用");

        // #420 標的兩種黏著狀態。
        for (status, want) in [("needs_login", "needs_login"), ("waiting_quota", "waiting_quota")] {
            roles::set_status(&app.db, Role::Responder, status, None, None).await.unwrap();
            assert_eq!(responder_unavailable(&app).await, Some(want), "status={status}");
        }
        // 清掉就回到可用。
        roles::set_status(&app.db, Role::Responder, "", None, None).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, None);

        // bot 被軟刪：**run 還掛著 running 也算不可用**。我原本回報這是個缺口（role_state 判的是
        // `row.bot_id` 的 active run，不是那顆 bot 還在不在），287c772a 收掉了——刪掉的協調者裁示不了
        // 任何東西，閘門說「可以裁示」就等於核准永遠不會被改派。這一條釘住那個修法。
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='resp'").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "軟刪但 run 還在 running");
        // run 也收掉之後一樣（原本就會走到的那條）。
        sqlx::query("UPDATE runs SET state='stopped' WHERE id='r1'").execute(&app.db).await.unwrap();
        assert_eq!(responder_unavailable(&app).await, Some("no_run"), "登記過但沒有在跑的 run");
    }

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("agm-failover-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("test.sqlite")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
        store::get_or_init(&app.db).await.unwrap();
        app
    }

    /// 寫一則協調者手上的 `approval_requested`，`created_at` 往回調 `age` 秒。
    async fn approval_event(app: &Arc<App>, key: &str, age: i64) -> String {
        let id = store::push_inbox(&app.db, key, "approval_requested", None, None, None, &json!({"id": key})).await.unwrap().unwrap();
        sqlx::query("UPDATE supervisor_inbox SET role='responder', wake=1, created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-age))
            .bind(&id)
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn payload(app: &Arc<App>, id: &str) -> Value {
        let s: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap();
        serde_json::from_str(&s).unwrap()
    }

    /// 票上的主線：協調者不可用、核准開著超過 5 分鐘 → 改派巡檢並叫醒它。
    #[tokio::test]
    async fn an_unavailable_responder_hands_its_stale_approvals_to_patrol() {
        let app = app().await;
        let old = approval_event(&app, "approval:a-old:requested", 600).await;
        let fresh = approval_event(&app, "approval:a-fresh:requested", 60).await;

        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1, "只動等超過 5 分鐘的那則");
        assert_eq!(owner_of(&app.db, &old).await.unwrap().as_deref(), Some("patrol"));
        assert_eq!(owner_of(&app.db, &fresh).await.unwrap().as_deref(), Some("responder"), "剛進來的留在協調者手上");

        let p = payload(&app, &old).await;
        assert_eq!(p["reassigned_from"], json!("responder"));
        assert_eq!(p["reassigned_reason"], json!("needs_login"));
        assert!(p["reassigned_at"].as_str().is_some_and(|s| !s.is_empty()));

        // 叫得醒：巡檢的待辦撈得到，而且 wake=1。
        let due = roles::due_for(&app.db, Role::Patrol, true, &crate::db::now(), 5).await.unwrap();
        assert_eq!(due.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec![old.as_str()]);
        assert_eq!(due[0].wake, Some(1));
    }

    /// 5 分鐘那條線**走真實路徑**測（i407 review 2026-09-24）：299 秒不動、300 秒動。
    /// 以前只有 `should_reassign` 的純函式測試釘這個邊界，而生產路徑的門檻寫在 SQL 裡、沒人測到——
    /// 把 SQL 的 cutoff 正負號弄反照樣全綠。現在門檻只有一份（`should_reassign`），而這條從
    /// `reassign_stale_approvals` 進去，所以那份真的被跑到。
    #[tokio::test]
    async fn the_five_minute_line_is_exact_on_the_path_that_actually_runs() {
        let app = app().await;
        let just_under = approval_event(&app, "approval:a-299:requested", REASSIGN_AFTER_SECS - 1).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0, "299 秒還不動");
        assert_eq!(owner_of(&app.db, &just_under).await.unwrap().as_deref(), Some("responder"));

        let exactly = approval_event(&app, "approval:a-300:requested", REASSIGN_AFTER_SECS).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1, "滿 300 秒就動");
        assert_eq!(owner_of(&app.db, &exactly).await.unwrap().as_deref(), Some("patrol"));
        // 299 秒那則還在原地：這一輪只動滿門檻的那一則。
        assert_eq!(owner_of(&app.db, &just_under).await.unwrap().as_deref(), Some("responder"));
    }

    /// 協調者恢復之後**不搶回**。這條靠 `roles::classify` 只補 `role IS NULL` 的列才成立，
    /// 所以直接跑 classify 一次釘住：以後有人把它改成「重新分類全部」時這裡要紅。
    #[tokio::test]
    async fn a_recovered_responder_does_not_take_the_approval_back() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 600).await;
        reassign_stale_approvals(&app, Some("needs_login")).await;
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("patrol"));

        // 協調者恢復（unavailable = None），而且分類又跑了好幾拍。
        for _ in 0..3 {
            assert_eq!(reassign_stale_approvals(&app, None).await, 0);
            roles::classify(&app.db).await.unwrap();
        }
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("patrol"), "恢復之後不能搶回去");
        // 路由表自己也要答巡檢（payload 帶著 to_role）。
        let p = payload(&app, &id).await;
        assert_eq!(roles::route("approval_requested", &p, None).role, Role::Patrol);
    }

    /// 協調者可用、或狀態讀不到（兩者都是 `None`）→ 一則都不動。
    #[tokio::test]
    async fn nothing_moves_while_the_responder_is_fine_or_unknown() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 9000).await;
        assert_eq!(reassign_stale_approvals(&app, None).await, 0);
        assert_eq!(owner_of(&app.db, &id).await.unwrap().as_deref(), Some("responder"));
    }

    /// 只有核准會改派：bot 的申請與 mission 事件留在協調者佇列（SPEC §18.15）。
    #[tokio::test]
    async fn only_approvals_are_reassigned() {
        let app = app().await;
        let mut ids = vec![];
        for kind in ["bot_request", "mission_question", "mission_created"] {
            let id = store::push_inbox(&app.db, &format!("k-{kind}"), kind, None, None, None, &json!({})).await.unwrap().unwrap();
            sqlx::query("UPDATE supervisor_inbox SET role='responder', wake=1, created_at=? WHERE id=?")
                .bind(crate::db::iso_in(-9000))
                .bind(&id)
                .execute(&app.db)
                .await
                .unwrap();
            ids.push(id);
        }
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0);
        for id in &ids {
            assert_eq!(owner_of(&app.db, id).await.unwrap().as_deref(), Some("responder"), "{id} 不該被改派");
        }
    }

    /// 協調者累積的補送次數不能跟著搬過去：巡檢的 `due_for` 有 `notify_attempts < max` 的上限，
    /// 帶著 5 次過去等於改派的同一刻就已經超過上限，巡檢永遠撈不到它。
    #[tokio::test]
    async fn the_responders_failed_attempts_do_not_count_against_patrol() {
        let app = app().await;
        let id = approval_event(&app, "approval:a1:requested", 600).await;
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=5, notify_error='x', delivered_at=?, state='delivered', notify_turn_id='t1' WHERE id=?")
            .bind(crate::db::now())
            .bind(&id)
            .execute(&app.db)
            .await
            .unwrap();

        assert_eq!(reassign_stale_approvals(&app, Some("no_run")).await, 1);
        let (attempts, state, turn): (i64, String, Option<String>) =
            sqlx::query_as("SELECT notify_attempts, state, notify_turn_id FROM supervisor_inbox WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap();
        assert_eq!((attempts, state.as_str(), turn), (0, "pending", None));
        let due = roles::due_for(&app.db, Role::Patrol, true, &crate::db::now(), 5).await.unwrap();
        assert_eq!(due.len(), 1, "巡檢撈得到它");
    }

    /// 同一則跑兩拍只會改派一次（第二拍的 SQL 條件已經不成立）。
    #[tokio::test]
    async fn reassigning_twice_is_a_no_op() {
        let app = app().await;
        approval_event(&app, "approval:a1:requested", 600).await;
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 1);
        assert_eq!(reassign_stale_approvals(&app, Some("needs_login")).await, 0, "第二拍不再動它");
    }

    /// 30 分鐘沒裁示就報出來，而且用**接過來的等待起點**算：重申請取代舊的那筆時等待是接續的，
    /// 從新 id 的 `created_at` 重新算會讓「已經等兩小時」看起來像剛剛才申請。
    #[tokio::test]
    async fn an_approval_nobody_decided_for_thirty_minutes_is_reported() {
        let app = app().await;
        let fresh = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c1"), None, None).await.unwrap().approval.id;
        assert!(stalled_approvals(&app, STALLED_AFTER_SECS).await.unwrap().is_empty(), "剛申請的不算");

        sqlx::query("UPDATE supervisor_approvals SET created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-STALLED_AFTER_SECS - 60))
            .bind(&fresh)
            .execute(&app.db)
            .await
            .unwrap();
        let out = stalled_approvals(&app, STALLED_AFTER_SECS).await.unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, fresh);
        assert!(out[0].1 >= STALLED_AFTER_SECS);
        assert_eq!(out[0].2, "kick");

        // 裁示掉就不再報（incidents 那條路會自動 resolve）。
        store::decide_approval(&app.db, &fresh, "approved", "AGM", None, None).await.unwrap();
        assert!(stalled_approvals(&app, STALLED_AFTER_SECS).await.unwrap().is_empty(), "裁示過的不算");
    }

    /// 接續的等待起點（`wait_since`）優先於新那筆的 `created_at`。
    #[tokio::test]
    async fn a_superseding_request_keeps_the_original_wait_for_the_stall_clock() {
        let app = app().await;
        let first = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c1"), None, None).await.unwrap().approval.id;
        sqlx::query("UPDATE supervisor_approvals SET created_at=?, wait_since=? WHERE id=?")
            .bind(crate::db::iso_in(-7200))
            .bind(crate::db::iso_in(-7200))
            .bind(&first)
            .execute(&app.db)
            .await
            .unwrap();
        // 重申請：自動取代掉上面那筆，並把等待起點接過來。
        let second = store::create_approval(&app.db, "kick", "rebuild", "s", Some("c2"), None, None).await.unwrap().approval.id;
        let out = stalled_approvals(&app, STALLED_AFTER_SECS).await.unwrap();
        assert_eq!(out.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), vec![second.as_str()], "只剩新那筆 pending");
        assert!(out[0].1 >= 7200, "等待是接續的，不是從新 id 重新算：{}", out[0].1);
    }
}
