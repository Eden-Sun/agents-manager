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

use crate::lifecycle::LcError;
use crate::state::App;
use crate::supervisor::store::Assignment;
use serde_json::json;
use std::sync::Arc;

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

/// 派新交辦之前：這個任務不能已經有一件開著的。
///
/// 回 409 `mission_busy`，附上開著的那件（AGM 要嘛等它結案，要嘛對它 `review followup`）。
/// **followup 不走這裡**：`review_with_followup` 在同一個交易裡把原件標成 `superseded`（終局）
/// 再開新的，任何一刻都只有一件開著，所以換手／退回那條路不受影響。
pub async fn ensure_can_assign(app: &Arc<App>, mission_id: &str, role: &str) -> Result<(), LcError> {
    let open = open_assignments(app, mission_id).await?;
    if let Some(a) = open.first() {
        return Err(LcError::conflict(
            "mission_busy",
            json!({
                "mission_id": mission_id,
                "requested_role": role,
                "open_assignments": open.iter().map(brief).collect::<Vec<_>>(),
                "hint": format!(
                    "這個任務已經有一件開著的交辦（{}／{}）：等它結案，或對它 `review followup`，不要同時開第二件",
                    a.mission_role.as_deref().unwrap_or("?"),
                    a.status
                ),
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
    #[tokio::test]
    async fn settled_assignments_do_not_block_anything() {
        let env = tt::env().await;
        for (i, st) in crate::supervisor::assignment_state::TERMINAL.iter().enumerate() {
            let id = mission(&env, &format!("m-term-{i}")).await;
            linked(&env.app, &id, &format!("t-{i}"), "executor", st).await;
            assert!(ensure_can_assign(&env.app, &id, "reviewer").await.is_ok(), "{st} 不該擋派工");
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
            let busy_for_assign = ensure_can_assign(&env.app, &id, "reviewer").await.is_err();
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
}
