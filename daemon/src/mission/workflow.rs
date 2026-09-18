//! 任務流程裡**確定性**的那幾條規則（issue #74 第一刀）。
//!
//! SPEC §18.14 開頭就寫著「一個任務同時只有一件開著的交辦；`phase` 由交辦推導，AGM 不另存狀態」，
//! 第 9 條又寫一次「不在一個任務裡同時開兩件交辦」。**但 daemon 從來沒有擋過。** 它是一條靠 AGM
//! 記得的規則——而 issue #74 抱怨的正是這個：workflow correctness 依賴 LLM 記得規則，不是 domain
//! state machine 保證的。
//!
//! 這條特別要緊，因為 `mission::api::phase` 是「取最後一件還開著的交辦的角色」。同時開兩件的時候
//! 那個「最後一件」就不是一個定義良好的東西：任務卡、`mission get`、AGM 的下一步判斷讀到的都是
//! 同一個函式，但它答什麼取決於哪一件排在後面。**先把「同時只有一件」變成事實，`phase` 才是個函式。**
//!
//! 刻意只搬確定性的部分。需要人判斷的（`ask_user`、`no_independent_reviewer`、findings 怎麼寫、
//! 要不要再一輪）留在 AGM 那邊，這裡一個字都不碰。
//!
//! 跟 ownership 衝突的差別要講清楚：那個是「**回報**，從不強制」，因為 daemon 沒辦法知道兩個模組
//! 是不是真的獨立，用字串比對去擋工作是錯的（SPEC §18.4，`supervisor::mod` 裡有註解）。這裡不一樣——
//! 「這個任務現在有幾件交辦開著」是一個查得到的事實，不是猜的。
//!
//! 第二刀（issue #74 其餘驗收）：下一步由 [`super::flow`] 從持久狀態推導，這裡把它接到入口上——
//! 派工的角色順序（`out_of_order`）、結案對交付的要求（[`delivery_record`]），以及任務停在「輪到 AGM」
//! 卻沒有人會叫醒它時的接續（[`wake_stalled`]，daemon 重啟後也是靠這條接回去）。

use super::flow;
use crate::lifecycle::LcError;
use crate::mission::store::MissionEvent;
use crate::state::App;
use crate::supervisor::store::Assignment;
use serde_json::{json, Value};
use std::sync::Arc;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 推導要的兩份清單：這個任務的交辦與事件，都是寫入順序。
pub async fn inputs(app: &Arc<App>, mission_id: &str) -> Result<(Vec<Assignment>, Vec<MissionEvent>), LcError> {
    let assignments = crate::supervisor::store::mission_assignments(&app.db, mission_id).await.map_err(up)?;
    let events = crate::mission::store::events(&app.db, mission_id).await.map_err(up)?;
    Ok((assignments, events))
}

/// 任務的下一步與流程摘要（`mission get`、裁示回應用）。讀不到任務就是 `null`。
pub async fn next_json(app: &Arc<App>, mission_id: &str) -> Value {
    let Ok(Some(m)) = crate::mission::store::get(&app.db, mission_id).await else { return Value::Null };
    let Ok((assignments, events)) = inputs(app, mission_id).await else { return Value::Null };
    let f = flow::derive(&assignments, &events);
    json!({"mission_id": m.id, "next": f.next(&m), "flow": f.summary()})
}

/// 這個任務底下還開著的交辦（`OPEN_STATES`），照寫入順序。
pub async fn open_assignments(app: &Arc<App>, mission_id: &str) -> Result<Vec<Assignment>, LcError> {
    Ok(crate::supervisor::store::mission_assignments(&app.db, mission_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .into_iter()
        .filter(|a| a.is_open())
        .collect())
}

fn brief(a: &Assignment) -> serde_json::Value {
    json!({"id": a.id, "role": a.mission_role, "status": a.status, "target_bot_id": a.target_bot_id})
}

/// 派新交辦之前：這個任務不能已經有一件開著的，而且角色要排得上（[`flow::Flow::allowed_roles`]）。
///
/// 回 409 `mission_busy`，附上開著的那件（AGM 要嘛等它結案，要嘛對它 `review followup`）；
/// 角色排不上回 409 `out_of_order`，附上 `next`。
/// **followup 不走這裡**：`review_with_followup` 在同一個交易裡把原件標成 `superseded`（終局）
/// 再開新的，任何一刻都只有一件開著，所以換手／退回那條路不受影響；角色沿用原件，也不改變流程走到哪。
///
/// 呼叫端（`supervisor::assign`）握著 supervisor 鎖，所以這裡也是「任務還收不收新交辦」算數的那一次
/// （[`crate::supervisor::api::mission_gate`]）：`post_assignment` 在鎖外查過，但排隊等鎖的時候任務可能已經被
/// 取消或結案——`mission cancel`／`complete` 關任務那一步也拿同一把鎖（issue #119）。
pub async fn ensure_can_assign(app: &Arc<App>, mission_id: &str, role: &str) -> Result<(), LcError> {
    let m = crate::mission::store::get(&app.db, mission_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("mission".into()))?;
    crate::supervisor::api::mission_gate(&m)?;
    let (assignments, events) = inputs(app, mission_id).await?;
    let open: Vec<&Assignment> = assignments.iter().filter(|a| a.is_open()).collect();
    if let Some(a) = open.first() {
        return Err(LcError::conflict(
            "mission_busy",
            json!({
                "mission_id": mission_id,
                "requested_role": role,
                "open_assignments": open.iter().copied().map(brief).collect::<Vec<_>>(),
                "hint": format!(
                    "這個任務已經有一件開著的交辦（{}／{}）：等它結案，或對它 `review followup`，不要同時開第二件",
                    a.mission_role.as_deref().unwrap_or("?"),
                    a.status
                ),
            }),
        ));
    }
    // 角色順序：這一代還沒有被接受的執行成果時，不派 reviewer／驗證者（規則在 `flow::Flow::allowed_roles`）。
    let f = flow::derive(&assignments, &events);
    if !f.allowed_roles().contains(&role) {
        return Err(LcError::conflict(
            "out_of_order",
            json!({
                "mission_id": mission_id,
                "requested_role": role,
                "stage": f.stage,
                "generation": f.generation,
                "allowed_roles": f.allowed_roles(),
                "next": f.step(),
                "hint": "這一代還沒有被接受的執行成果（第一次派工、或退回之後還沒重做）：先派執行者，審查／驗證等執行者的交辦被 accept 之後再派",
            }),
        ));
    }
    Ok(())
}

/// 結案之前：底下不能還有開著的交辦。
///
/// 以前 `post_complete` 除了「任務還開著」以外**什麼都不查**，全靠呼叫端照順序操作（issue #74 說的
/// 「依賴 runbook」）。結案時還有交辦開著的話，那顆 bot 會繼續做一件已經關掉的任務，回合結束時
/// `settle` 還會為它推一則 `assignment_completed` 給 AGM——一件沒有人要的工作，加一則沒有人看得懂的通知。
///
/// 取消那條路本來就會把底下的交辦逐件 `cancel`（§18.14 第 8 條），結案卻不會；這裡把兩邊對齊成
/// 「要嘛先收乾淨，要嘛走取消」。
pub async fn ensure_can_complete(app: &Arc<App>, mission_id: &str) -> Result<(), LcError> {
    let open = open_assignments(app, mission_id).await?;
    if !open.is_empty() {
        return Err(LcError::conflict(
            "assignments_open",
            json!({
                "mission_id": mission_id,
                "open_assignments": open.iter().map(brief).collect::<Vec<_>>(),
                "hint": "底下還有開著的交辦：先 `review accept`／`fail`／`cancel` 收乾淨再結案，或用 `mission cancel`（那條會自動逐件取消）",
            }),
        ));
    }
    Ok(())
}

/// 結案對交付的要求（規則在 [`flow::delivery_requirement`]）。回傳要寫進 `completed` 事件的記錄，以及
/// `no_changes` 要不要再附執行者的工作樹來證明（git 的檢查在 api 那一側做，這裡只判事件）。
pub async fn delivery_record(app: &Arc<App>, mission_id: &str, waiver: Option<flow::Waiver>) -> Result<(Value, bool), LcError> {
    let (assignments, events) = inputs(app, mission_id).await?;
    let f = flow::derive(&assignments, &events);
    let record = flow::delivery_requirement(&f, &events, waiver).map_err(|(reason, detail)| {
        let mut body = detail;
        body["mission_id"] = mission_id.into();
        body["next"] = json!(f.step());
        LcError::conflict(reason, body)
    })?;
    let needs_proof = record["reason"] == "no_changes" && flow::needs_worktree_proof(&assignments);
    Ok((record, needs_proof))
}

/// 任務停在「輪到 AGM」多久沒動靜就叫醒它。
pub const STALL_SECS: i64 = 600;

static LAST_SWEEP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// controller 每個 tick 呼叫；自己節流成一分鐘一次。
pub async fn wake_stalled(app: &Arc<App>) {
    let now = chrono::Utc::now();
    let last = LAST_SWEEP.load(std::sync::atomic::Ordering::Relaxed);
    if now.timestamp() - last < 60 {
        return;
    }
    LAST_SWEEP.store(now.timestamp(), std::sync::atomic::Ordering::Relaxed);
    wake_stalled_at(app, now).await;
}

/// 任務停在「輪到 AGM 動手」的那幾步（派下一個角色、記驗證結果、交付、結案）超過 [`STALL_SECS`]，
/// 而且沒有任何會叫醒 AGM 的東西在路上：推一則 `mission_next`，payload 帶推導出來的 `next`。
///
/// 這是「daemon 重啟後從持久狀態接續」的那一半：推導本身是純函式（[`flow`]），叫醒靠這裡。重啟、AGM 的
/// 回合被中斷、AGM 忘了——三種情況看起來都一樣：交辦都結案了、任務還開著、很久沒有人動它。
///
/// 不叫醒的：暫停中（在等人，AGM 已經問過了）、有交辦開著（在跑的等回合結束的通知；在等裁示的有
/// `assignment_completed` 與 `assignment_stalled`）、這個任務還有沒處理完的 inbox（例如 `mission_created`
/// 還沒被看到——再推一則只是重複）。同一步只叫一次：event_key 是那一步的簽名，全部來自持久狀態，重啟後
/// 算出來也一樣，所以 AGM 看過不理，不會每分鐘再叫一次。回傳這次叫醒了哪些任務。
pub async fn wake_stalled_at(app: &Arc<App>, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let Ok(missions) = crate::mission::store::open_unpaused(&app.db).await else { return Vec::new() };
    let mut woke = Vec::new();
    for m in missions {
        let Ok((assignments, events)) = inputs(app, &m.id).await else { continue };
        let f = flow::derive(&assignments, &events);
        let next = f.next(&m);
        if !next.is_agm_turn() {
            continue;
        }
        let since = assignments.iter().map(|a| a.updated_at.as_str()).chain([m.updated_at.as_str()]).max().unwrap_or_default().to_string();
        let idle = chrono::DateTime::parse_from_rfc3339(&since).map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds()).unwrap_or(0);
        if idle < STALL_SECS || outstanding_inbox(app, &m.id).await {
            continue;
        }
        let key = format!("mission:{}:next:{}", m.id, next.signature(f.generation, assignments.len()));
        // `text`／`message`／`action` 是喚醒摘要（`digest_text::detail` 的預設分支）會印的三欄：
        // AGM 在摘要裡就看得到是哪個任務、停多久、下一步做什麼，不必先打開 payload。
        let payload = json!({
            "mission_id": m.id,
            "project_id": m.project_id,
            "text": format!("任務 {}：{}", m.id, m.text.chars().take(120).collect::<String>()),
            "message": format!("停在「輪到 AGM」這一步 {} 分鐘：沒有交辦在跑、也沒有沒處理的通知（daemon 重啟或 AGM 回合中斷之後由這裡接回；同一步只提醒這一次）", idle / 60),
            "action": format!("next={}{}：{}", next.action, next.role.as_deref().map(|r| format!("（{r}）")).unwrap_or_default(), next.hint),
            "next": next,
            "flow": f.summary(),
            "idle_since": since,
            "idle_minutes": idle / 60,
        });
        match crate::supervisor::store::push_inbox(&app.db, &key, "mission_next", None, None, None, &payload).await {
            Ok(Some(_)) => woke.push(m.id.clone()),
            Ok(None) => {}
            Err(e) => tracing::warn!(mission = %m.id, error = %e, "could not queue mission_next"),
        }
    }
    woke
}

/// 這個任務還有沒處理完（不是 `handled`）的 inbox：任務自己的通知，或它底下交辦的通知。
async fn outstanding_inbox(app: &Arc<App>, mission_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM supervisor_inbox
          WHERE supervisor_id = ? AND state != 'handled'
            AND (json_extract(payload_json, '$.mission_id') = ?
                 OR assignment_id IN (SELECT id FROM supervisor_assignments WHERE mission_id = ?))",
    )
    .bind(crate::supervisor::store::SUPERVISOR_ID)
    .bind(mission_id)
    .bind(mission_id)
    .fetch_one(&app.db)
    .await
    .map(|n| n > 0)
    // 查不到就當有：寧可少叫一次（下一分鐘再看），不要在資料庫出狀況時亂叫。
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::store;
    use crate::testing as tt;

    fn conflict_body(e: LcError) -> serde_json::Value {
        match e {
            LcError::Conflict(v) => v,
            other => panic!("要是 409，不是 {other:?}"),
        }
    }

    /// 建一筆掛在任務下的交辦，狀態由呼叫端決定。
    async fn linked(app: &Arc<App>, mission_id: &str, crid: &str, role: &str, status: &str) -> String {
        store::get_or_init(&app.db).await.unwrap();
        let a = store::insert_assignment(&app.db, None, "bot1", crid, "做事", &[], None, true).await.unwrap();
        store::set_mission_link(&app.db, &a.id, mission_id, role).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?")
            .bind(status)
            .bind(&a.id)
            .execute(&app.db)
            .await
            .unwrap();
        a.id
    }

    /// **每次都要是新的一筆任務**：`create_announced` 對同一個 crid 會回既有那筆，共用 crid 的話
    /// 迴圈裡的交辦會疊在同一個任務上，第二圈之後「忙」是前一圈留下來的，測試就白測了。
    async fn mission(env: &tt::Env, crid: &str) -> String {
        let (m, _) = crate::mission::store::create_announced(
            &env.app.db,
            &crate::mission::store::NewMission {
                project_id: &env.project_id,
                client_request_id: crid,
                text: "做一件事",
                delivery_mode: "pr",
                executor_kind: "claude",
                on_5h_limit: "wait",
                max_rounds: 2,
                parent_mission_id: None,
            },
            |_| json!({}),
        )
        .await
        .unwrap();
        m.id
    }

    /// SPEC §18.14：一個任務同時只有一件開著的交辦。
    #[tokio::test]
    async fn a_mission_cannot_have_two_open_assignments() {
        let env = tt::env().await;
        let id = mission(&env, "m-two-open").await;
        linked(&env.app, &id, "c1", "executor", "delivered").await;
        let err = ensure_can_assign(&env.app, &id, "reviewer").await.unwrap_err();
        let body = conflict_body(err);
        assert_eq!(body["reason"], "mission_busy");
        assert_eq!(body["requested_role"], "reviewer");
        assert_eq!(body["open_assignments"][0]["role"], "executor");
    }

    /// 前一件結案之後就派得出去了——這是正常的流程，不能被擋住。
    #[tokio::test]
    async fn the_next_role_can_be_assigned_once_the_previous_one_is_settled() {
        let env = tt::env().await;
        let id = mission(&env, "m-next-role").await;
        let first = linked(&env.app, &id, "c1", "executor", "delivered").await;
        assert!(ensure_can_assign(&env.app, &id, "reviewer").await.is_err());
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE id=?")
            .bind(&first)
            .execute(&env.app.db)
            .await
            .unwrap();
        assert!(ensure_can_assign(&env.app, &id, "reviewer").await.is_ok(), "結案之後要派得出去");
    }

    /// 每一個未結案狀態都算「開著」，包括在等額度與被擋下的。
    #[tokio::test]
    async fn every_open_state_counts_as_busy() {
        let env = tt::env().await;
        for (i, st) in crate::supervisor::store::OPEN_STATES.iter().enumerate() {
            let id = mission(&env, &format!("m-open-{i}")).await;
            linked(&env.app, &id, &format!("c-{i}"), "executor", st).await;
            assert!(ensure_can_assign(&env.app, &id, "verifier").await.is_err(), "{st} 也算開著");
            assert!(ensure_can_complete(&env.app, &id).await.is_err(), "{st} 也擋結案");
        }
    }

    /// 終局的交辦不算開著，兩個入口都要放行。
    ///
    /// 派的是執行者：它在任何一關都排得上（重做、rebase、沒做完再派），所以這條只量「忙不忙」，不受角色
    /// 順序影響——reviewer 在執行者 `failed` 之後排不上是另一條規則（`out_of_order`），另外測。
    #[tokio::test]
    async fn settled_assignments_do_not_block_anything() {
        let env = tt::env().await;
        for (i, st) in crate::supervisor::assignment_state::TERMINAL.iter().enumerate() {
            let id = mission(&env, &format!("m-term-{i}")).await;
            linked(&env.app, &id, &format!("t-{i}"), "executor", st).await;
            assert!(ensure_can_assign(&env.app, &id, "executor").await.is_ok(), "{st} 不該擋派工");
            assert!(ensure_can_complete(&env.app, &id).await.is_ok(), "{st} 不該擋結案");
        }
    }

    /// **同一個狀態，兩個入口答案一致**：`assign` 與 `complete` 讀的是同一份「還開著的交辦」。
    ///
    /// issue #74 的病灶就是「同一個狀態在不同入口推出不同結果」，所以這條直接釘住兩邊同源。
    #[tokio::test]
    async fn both_entry_points_agree_on_whether_the_mission_is_busy() {
        let env = tt::env().await;
        for (i, st) in crate::supervisor::store::OPEN_STATES
            .iter()
            .chain(crate::supervisor::assignment_state::TERMINAL.iter())
            .enumerate()
        {
            let id = mission(&env, &format!("m-both-{i}")).await;
            linked(&env.app, &id, &format!("b-{i}"), "executor", st).await;
            let busy_for_assign = ensure_can_assign(&env.app, &id, "executor").await.is_err();
            let busy_for_complete = ensure_can_complete(&env.app, &id).await.is_err();
            assert_eq!(busy_for_assign, busy_for_complete, "{st}：兩個入口對「忙不忙」必須同一個答案");
        }
    }

    /// 沒有任何交辦的任務：兩邊都放行（例如根本還沒派工就被取消／結案）。
    #[tokio::test]
    async fn a_mission_with_no_assignments_is_not_busy() {
        let env = tt::env().await;
        let id = mission(&env, "m-empty").await;
        assert!(ensure_can_assign(&env.app, &id, "executor").await.is_ok());
        assert!(ensure_can_complete(&env.app, &id).await.is_ok());
    }

    /// 角色順序（issue #74）：這一代還沒有被接受的執行成果，就不派 reviewer／驗證者；退回之後也一樣，
    /// 執行者重做之前不能重審舊的那份。409 附上推導出來的 `next`，AGM 照著做就對了。
    #[tokio::test]
    async fn reviewers_and_verifiers_wait_for_an_accepted_executor_in_this_generation() {
        let env = tt::env().await;
        let id = mission(&env, "m-order").await;
        for role in ["reviewer", "verifier"] {
            let body = conflict_body(ensure_can_assign(&env.app, &id, role).await.unwrap_err());
            assert_eq!(body["reason"], "out_of_order", "{role}");
            assert_eq!((body["next"]["action"].as_str(), body["next"]["role"].as_str()), (Some("assign"), Some("executor")));
        }
        // 執行者沒做完（failed）也還沒有成果。
        linked(&env.app, &id, "o1", "executor", "failed").await;
        assert_eq!(conflict_body(ensure_can_assign(&env.app, &id, "reviewer").await.unwrap_err())["reason"], "out_of_order");
        // 被接受之後：審查、驗證（跳過審查）都排得上。
        linked(&env.app, &id, "o2", "executor", "completed").await;
        assert!(ensure_can_assign(&env.app, &id, "reviewer").await.is_ok());
        assert!(ensure_can_assign(&env.app, &id, "verifier").await.is_ok());

        // 退回：新的一代，又回到只能派執行者。走真的入口寫 round（錨點由它記）。
        let _ = crate::mission::api::post_round(axum::extract::State(env.app.clone()), axum::extract::Path(id.clone())).await.unwrap();
        let events = crate::mission::store::events(&env.app.db, &id).await.unwrap();
        let round: Value = serde_json::from_str(&events.iter().find(|e| e.kind == "round").unwrap().payload_json).unwrap();
        let last = store::mission_assignments(&env.app.db, &id).await.unwrap().last().unwrap().id.clone();
        assert_eq!(round[flow::ANCHOR], json!(last), "round 記下當時最後一件交辦：之後派的才算新的一代，不靠毫秒時間戳");
        let body = conflict_body(ensure_can_assign(&env.app, &id, "reviewer").await.unwrap_err());
        assert_eq!((body["reason"].as_str(), body["generation"].as_u64()), (Some("out_of_order"), Some(1)));
        assert_eq!(body["next"]["rework"], true, "退回之後的執行者是重做");
        assert!(ensure_can_assign(&env.app, &id, "executor").await.is_ok());
    }

    /// 停在輪到 AGM 的一步、很久沒動靜：推一則 `mission_next`，同一步只推一次；還沒到時間、有交辦開著、
    /// 暫停中、或還有沒處理的通知，都不推。這是「daemon 重啟後從持久狀態接續」的叫醒那一半。
    #[tokio::test]
    async fn a_mission_idle_on_the_managers_step_wakes_it_once() {
        let env = tt::env().await;
        let app = &env.app;
        store::get_or_init(&app.db).await.unwrap();
        let id = mission(&env, "m-stall").await;
        // 建任務時推的 mission_created 已經處理過（不然「還有沒處理的通知」會先擋下來）。
        sqlx::query("UPDATE supervisor_inbox SET state='handled'").execute(&app.db).await.unwrap();
        linked(app, &id, "s1", "executor", "completed").await;
        let now = chrono::Utc::now();
        let later = now + chrono::Duration::seconds(STALL_SECS + 5);
        async fn next_rows(app: &Arc<App>) -> Vec<(String, String)> {
            sqlx::query_as("SELECT event_key, payload_json FROM supervisor_inbox WHERE kind='mission_next' ORDER BY rowid")
                .fetch_all(&app.db)
                .await
                .unwrap()
        }

        assert!(wake_stalled_at(app, now).await.is_empty(), "剛動過，不叫");
        assert_eq!(wake_stalled_at(app, later).await, vec![id.clone()]);
        let rows = next_rows(app).await;
        assert_eq!(rows.len(), 1);
        let payload: Value = serde_json::from_str(&rows[0].1).unwrap();
        assert_eq!((payload["next"]["action"].as_str(), payload["next"]["role"].as_str()), (Some("assign"), Some("reviewer")));
        assert_eq!(roles_of("mission_next"), (crate::supervisor::roles::Role::Responder, true));

        // AGM 看過（handled）但沒動：同一步不再叫。
        sqlx::query("UPDATE supervisor_inbox SET state='handled'").execute(&app.db).await.unwrap();
        assert!(wake_stalled_at(app, later + chrono::Duration::seconds(3600)).await.is_empty(), "同一步只叫一次");

        // 進到下一步（派了 reviewer、被接受）又停住：那是新的一步，再叫一次。
        linked(app, &id, "s2", "reviewer", "completed").await;
        let much_later = later + chrono::Duration::seconds(2 * STALL_SECS);
        assert_eq!(wake_stalled_at(app, much_later).await, vec![id.clone()]);
        let rows = next_rows(app).await;
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].0, rows[1].0);

        // 還有沒處理的通知：不疊一則。
        linked(app, &id, "s3", "verifier", "completed").await;
        crate::supervisor::store::push_inbox(&app.db, "x", "mission_question", None, None, None, &json!({"mission_id": id})).await.unwrap();
        assert!(wake_stalled_at(app, much_later + chrono::Duration::seconds(2 * STALL_SECS)).await.is_empty(), "有沒處理的通知就不疊");
        sqlx::query("UPDATE supervisor_inbox SET state='handled'").execute(&app.db).await.unwrap();

        // 有交辦開著（在跑）：不是 AGM 的一步。
        linked(app, &id, "s4", "executor", "delivered").await;
        assert!(wake_stalled_at(app, much_later + chrono::Duration::seconds(4 * STALL_SECS)).await.is_empty(), "交辦在跑");
        sqlx::query("UPDATE supervisor_assignments SET status='completed' WHERE client_request_id='s4'").execute(&app.db).await.unwrap();

        // 暫停中：在等人，不叫 AGM。
        crate::mission::store::pause(&app.db, &id, "clarify", None).await.unwrap();
        assert!(wake_stalled_at(app, much_later + chrono::Duration::seconds(6 * STALL_SECS)).await.is_empty(), "暫停中");
    }

    fn roles_of(kind: &str) -> (crate::supervisor::roles::Role, bool) {
        let r = crate::supervisor::roles::route(kind, &json!({}), None);
        (r.role, r.wake)
    }
}
