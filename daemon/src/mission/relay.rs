//! 交辦改狀態時，替它所屬的任務推一則 `mission_updated`（review3 c1 M5）。
//!
//! 任務卡的階段（執行／審查／等額度／等 AGM）全部從交辦推導，但交辦的建立、派送、`quota_blocked`、驗收
//! 只推 `supervisor_changed`：AGM 照 runbook `assign --mission` 之後，卡片仍停在「規劃」，中途撞 5h 的
//! 「等額度」也要重整才看得到。
//!
//! 不去改每一個寫交辦的地方（controller、supervisor API、assign 各自發事件），而是在這裡聽匯流排：
//! 帶 `assignment_id` 的 `supervisor_changed` 查一次那件交辦掛在哪個任務，有就補推。以後新增的交辦轉換
//! 只要照慣例發 `supervisor_changed`，任務卡就跟得上。

use crate::state::{App, WsEvent};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// 開機時呼叫一次。訂閱在 spawn **之前**完成，之後發的事件一則都不會漏在「還沒訂上」的空窗裡。
pub fn spawn(app: Arc<App>) {
    let mut rx = app.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    relay(&app, &ev).await;
                }
                // 落後就跳過那幾則：前端在重連與 resync 時會整份重抓已載入的任務（`refreshLoadedMissions`）。
                Err(RecvError::Lagged(n)) => tracing::debug!(skipped = n, "mission relay lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// 這則事件是某個任務的交辦改了狀態 → 推 `mission_updated`，回傳推了沒有。
pub async fn relay(app: &Arc<App>, ev: &WsEvent) -> bool {
    if ev.kind != "supervisor_changed" {
        return false;
    }
    let Some(aid) = ev.data.get("assignment_id").and_then(Value::as_str) else { return false };
    let Ok(Some(a)) = crate::supervisor::store::assignment(&app.db, aid).await else { return false };
    let Some(mid) = a.mission_id.as_deref() else { return false };
    let Ok(Some(m)) = super::store::get(&app.db, mid).await else { return false };
    app.emit(
        "mission_updated",
        json!({"mission_id": m.id, "project_id": m.project_id, "status": m.status(), "assignment_id": aid}),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn next_mission_update(rx: &mut tokio::sync::broadcast::Receiver<WsEvent>) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(ev)) if ev.kind == "mission_updated" => return Some(ev.data),
                Ok(Ok(_)) => continue,
                _ => return None,
            }
        }
    }

    /// 交辦的每一次轉換（這裡用派送前被額度擋下那種）都帶出所屬任務的 `mission_updated`；不屬於任務的不推。
    #[tokio::test]
    async fn an_assignment_changing_state_refreshes_its_mission_card() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let (m, _) = super::super::store::create(
            &app.db,
            &super::super::store::NewMission {
                project_id: &env.project_id,
                client_request_id: "relay",
                text: "做 X",
                delivery_mode: "pr",
                executor_kind: "claude",
                on_5h_limit: "wait",
                max_rounds: 2,
                parent_mission_id: None,
            },
        )
        .await
        .unwrap();
        let x = crate::testing::claude_bot(&app, &env.project_id, "x").await;
        let y = crate::testing::claude_bot(&app, &env.project_id, "y").await;
        let linked = crate::supervisor::store::insert_assignment(&app.db, None, &x.id, "crid-linked", "做 X", &[], None, true).await.unwrap();
        crate::supervisor::store::set_mission_link(&app.db, &linked.id, &m.id, "executor").await.unwrap();
        let loose = crate::supervisor::store::insert_assignment(&app.db, None, &y.id, "crid-loose", "別的事", &[], None, true).await.unwrap();

        spawn(app.clone());
        let mut rx = app.subscribe();

        app.emit("supervisor_changed", json!({"assignment_id": loose.id, "status": "delivered"})).await;
        app.emit("supervisor_changed", json!({"responder": "woken"})).await;
        assert!(next_mission_update(&mut rx).await.is_none(), "不屬於任務的交辦、跟交辦無關的事件都不推");

        app.emit("supervisor_changed", json!({"assignment_id": linked.id, "status": "quota_blocked"})).await;
        let data = next_mission_update(&mut rx).await.expect("任務卡要收到更新");
        assert_eq!(data["mission_id"], json!(m.id));
        assert_eq!(data["project_id"], json!(env.project_id));
        assert_eq!(data["assignment_id"], json!(linked.id));
    }
}
