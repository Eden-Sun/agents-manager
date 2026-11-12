//! SPEC §6.5a：被軟刪的子 agent 走既有的 `POST /bots/{id}/restore`（§10.4a）還原，不直接改 DB；
//! 子 agent 的 start／restart 一律 409 `child_restart_forbidden`。
use super::*;
use crate::lifecycle::restart_kind_tests::live_child;
use crate::testing as tt;

async fn soft_delete(app: &Arc<App>, id: &str) {
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE bot_id=?").bind(db::now()).bind(id).execute(&app.db).await.unwrap();
}

fn conflict_reason(err: LcError) -> String {
    match err {
        LcError::Conflict(v) => v["reason"].as_str().unwrap_or("").to_string(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_soft_deleted_child_is_restored_without_a_run() {
    let e = tt::env().await;
    let kid = live_child(&e, "pvw").await;
    soft_delete(&e.app, &kid.id).await;
    let resp = restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let b = db::bot(&e.app.db, &kid.id).await.unwrap().unwrap();
    assert!(b.deleted_at.is_none() && b.managed_by == "child" && b.parent_bot_id.is_some());
    assert!(db::active_run(&e.app.db, &kid.id).await.unwrap().is_none(), "restore 不開 run");
    // Herdr has not reopened the child yet; a reconcile in this window must preserve its row/history.
    *e.herdr.agents.lock().unwrap() = Vec::new();
    crate::reconcile::reconcile_host(&e.app, crate::config::LOCAL_HOST).await.unwrap();
    assert!(db::bot(&e.app.db, &kid.id).await.unwrap().unwrap().deleted_at.is_none(), "restore grace keeps the original child row");
}

#[tokio::test]
async fn an_unreopened_restored_child_is_retired_after_its_grace_expires() {
    let e = tt::env().await;
    let kid = live_child(&e, "pvw-expired").await;
    soft_delete(&e.app, &kid.id).await;
    restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap();
    *e.herdr.agents.lock().unwrap() = Vec::new();
    sqlx::query("UPDATE supervisor_notes SET body='2000-01-01T00:00:00.000Z' WHERE supervisor_id=? AND kind='child_retirement_grace'")
        .bind(&kid.id)
        .execute(&e.app.db)
        .await
        .unwrap();

    crate::reconcile::reconcile_host(&e.app, crate::config::LOCAL_HOST).await.unwrap();

    assert!(db::bot(&e.app.db, &kid.id).await.unwrap().unwrap().deleted_at.is_some(), "expired grace restores the existing retirement rule");
}

#[tokio::test]
async fn restore_refuses_a_live_child_and_a_taken_name() {
    let e = tt::env().await;
    let kid = live_child(&e, "dup").await;
    assert!(conflict_reason(restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap_err()).contains("not deleted"));
    soft_delete(&e.app, &kid.id).await;
    // 同名的另一顆活 child（例如父 bot 已經用 herdr 重開了一顆）。
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     SELECT ?, project_id, name, kind, '[]', 0, 0, 'tok2', 'child', parent_bot_id, ? FROM bots WHERE id=?")
        .bind(db::ulid()).bind(db::now()).bind(&kid.id).execute(&e.app.db).await.unwrap();
    assert!(conflict_reason(restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap_err()).contains("already in use"));
    assert!(matches!(restore_bot(State(e.app.clone()), Path("nope".into())).await.unwrap_err(), LcError::NotFound(_)));
}

/// 還原被 409 擋下（名字被同專案的活 bot 佔住）時不能先動任何東西：刪之前排著的 restart intent 還得是 pending。
/// 以前 `UPDATE intents … abandoned` 排在名字檢查之前，拒絕的還原照樣把它改掉（交易範圍）。
#[tokio::test]
async fn a_refused_restore_leaves_the_pending_restart_intent_alone() {
    let e = tt::env().await;
    let kid = live_child(&e, "dup2").await;
    soft_delete(&e.app, &kid.id).await;
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     SELECT ?, project_id, name, kind, '[]', 0, 0, 'tok3', 'child', parent_bot_id, ? FROM bots WHERE id=?")
        .bind(db::ulid()).bind(db::now()).bind(&kid.id).execute(&e.app.db).await.unwrap();
    sqlx::query("INSERT INTO intents (id, kind, subject_id, host, payload_json, status, created_at, updated_at, expires_at)
                     VALUES (?, 'restart', ?, 'local', '{}', 'pending', ?, ?, ?)")
        .bind(db::ulid()).bind(&kid.id).bind(db::now()).bind(db::now()).bind(db::iso_in(3600)).execute(&e.app.db).await.unwrap();

    assert!(conflict_reason(restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap_err()).contains("already in use"));
    let status: String = sqlx::query_scalar("SELECT status FROM intents WHERE kind='restart' AND subject_id=?").bind(&kid.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(status, "pending", "被擋下的還原不能改動 intent");
}

#[tokio::test]
async fn start_and_restart_refuse_children() {
    let e = tt::env().await;
    let kid = live_child(&e, "kid").await;
    let q = || Query(StartQuery { resume: None, session: None });
    assert_eq!(conflict_reason(restart_bot(State(e.app.clone()), Path(kid.id.clone()), q()).await.unwrap_err()), "child_restart_forbidden");
    assert_eq!(conflict_reason(start_bot(State(e.app.clone()), Path(kid.id.clone()), q()).await.unwrap_err()), "child_restart_forbidden");
    assert!(db::active_run(&e.app.db, &kid.id).await.unwrap().is_some(), "子 agent 的 run 一根毛都不能動");
}
