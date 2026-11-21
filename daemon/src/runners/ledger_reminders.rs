//! 承諾帳本去重提醒（ledger-phase1 T4，規格 §1.6）：每 60 秒掃一次。
//! 先回收孤兒承辦（承辦 bot 已刪、或已不是管理入口）；再找需要提醒的事項（未分流、已到期、待接手），
//! 依 `(item, stage, version)` 去重；同一位目標管理 bot 的待辦合併成一則，主動排一次管理回合。
use std::{collections::BTreeMap, hash::{Hash, Hasher}, sync::Arc};

use am_base::ledger::{self, Reminder, ESCALATE_AFTER_SECS};

use crate::{events::ports::TurnCommands, lifecycle::{LcError, QueueClass}, state::App};

/// 一則提醒最多列幾件，其餘寫「另 n 件」。
const MAX_LINES: usize = 5;
const NOTICE_PREFIX: &str = "ledger-reminder:";

/// 每輪入口（`s6_l::sweep_child_done` 呼叫）。出錯只記 log，不影響其他 sweep。
pub async fn sweep(app: &Arc<App>) {
    if let Err(e) = ledger::reclaim_orphaned_items(&app.db).await {
        tracing::warn!(error = ?e, "ledger orphan reclaim failed");
    }
    let due = match ledger::due_reminders(&app.db, chrono::Utc::now()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "ledger reminder scan failed");
            return;
        }
    };
    let mut by_target: BTreeMap<String, Vec<Reminder>> = BTreeMap::new();
    for r in due {
        if let Some(target) = target_for(app, &r).await {
            by_target.entry(target).or_default().push(r);
        }
    }
    for (target, items) in by_target {
        // 目標不在跑：不標記，下一輪再說。
        if !running(app, &target).await {
            continue;
        }
        // 先 CAS 標記（版本還是當初看到的才寫）；只有標記成功的才會送。
        let mut marked = Vec::new();
        for r in items {
            if matches!(ledger::mark_reminded(&app.db, &r).await, Ok(true)) {
                marked.push(r);
            }
        }
        if marked.is_empty() {
            continue;
        }
        let text = notice_text(&marked);
        let crid = format!("{NOTICE_PREFIX}{:016x}", hash_of(&marked));
        match app.prompt_relayed_queueable(&target, &text, &crid, None, QueueClass::DaemonNotice).await {
            Ok(_) => tracing::info!(target = %target, items = marked.len(), "ledger reminder queued"),
            // 管理者的佇列格被占著、或輸入框有草稿：還原標記，下一輪再提醒。
            Err(LcError::Conflict(_)) => unmark_all(app, &marked).await,
            Err(e) => {
                tracing::warn!(target = %target, error = ?e, "ledger reminder could not be queued");
                unmark_all(app, &marked).await;
            }
        }
    }
}

/// 通知對象（唯一）：有承辦人就只通知承辦人；孤兒，或副手的到期事項逾期超過 30 分鐘，升級給頂層管理者（頂層忙、代班中就給代班副手）。
async fn target_for(app: &Arc<App>, r: &Reminder) -> Option<String> {
    let overdue = r
        .due_at
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
        .is_some_and(|d| (chrono::Utc::now() - d.with_timezone(&chrono::Utc)).num_seconds() >= ESCALATE_AFTER_SECS);
    let escalate = r.owner_bot_id.is_none() || (r.stage == "due" && overdue);
    if !escalate {
        return r.owner_bot_id.clone();
    }
    let top = ledger::top_manager(&app.db, &r.project_id).await.ok().flatten()?;
    // 副手自己名下的到期事項逾期升級：一律給頂層管理者，絕不退回給副手本人（否則主忙時會被副手吃掉、主永遠收不到）。
    if r.owner_bot_id.is_some() {
        return Some(top);
    }
    // 孤兒（無承辦人）：頂層忙、且代班副手真的在代班（acting）且在跑，才給代班副手；其他情況給頂層。
    if busy(app, &top).await {
        let deputy: Option<String> = sqlx::query_scalar("SELECT deputy_bot_id FROM projects WHERE id=? AND mode='expert' AND deleted_at IS NULL")
            .bind(&r.project_id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten()
            .flatten();
        if let Some(d) = deputy {
            if crate::runners::relay_deputy::acting(app, &d).await && running(app, &d).await {
                return Some(d);
            }
        }
    }
    Some(top)
}

async fn running(app: &Arc<App>, bot_id: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE bot_id=? AND state='running')").bind(bot_id).fetch_one(&app.db).await.unwrap_or(false)
}

async fn busy(app: &Arc<App>, bot_id: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE bot_id=? AND state='running' AND agent_status='working')").bind(bot_id).fetch_one(&app.db).await.unwrap_or(false)
}

async fn unmark_all(app: &Arc<App>, items: &[Reminder]) {
    for r in items {
        if let Err(e) = ledger::unmark_reminded(&app.db, r).await {
            tracing::warn!(item = %r.item_id, error = ?e, "ledger reminder unmark failed");
        }
    }
}

fn hash_of(items: &[Reminder]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for r in items {
        r.item_id.hash(&mut h);
        r.version.hash(&mut h);
        r.stage.hash(&mut h);
    }
    h.finish()
}

/// 提醒文字（給管理回合的一則簡短待辦）。
fn notice_text(items: &[Reminder]) -> String {
    let mut lines = vec!["承諾帳本提醒（系統排的，不是使用者訊息）：以下事項需要你處理".to_string()];
    for r in items.iter().take(MAX_LINES) {
        let what = match r.stage {
            "untriaged" => "待分流".to_string(),
            "due" => "已到期".to_string(),
            _ => format!("沒有承辦人，請認領：agm ledger claim L{} --expect-version {}", r.code_no, r.version),
        };
        lines.push(format!("- L{}（{}）{}", r.code_no, what, r.title));
        // 有下一步就帶上（例如「使用者按了不知道」的複查指示），管理者下一輪照著做。
        if !r.next_step.trim().is_empty() {
            lines.push(format!("  下一步：{}", r.next_step.trim()));
        }
    }
    if items.len() > MAX_LINES {
        lines.push(format!("…另 {} 件", items.len() - MAX_LINES));
    }
    lines.push("逐件處理：回覆使用者、更新狀態，或交回給對的人。".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// 專家專案的頂層管理者（running、idle）。
    async fn manager(env: &crate::testing::Env) -> String {
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let m = crate::testing::claude_bot(&env.app, &env.project_id, "lg-mgr").await;
        sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&m.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &m.id).await;
        // 管理者手上有一個進行中的回合：提醒會排進佇列（沒有它，假 pane 的輸入框讀不到，送進去會 composer_unreadable）。
        let conv = crate::db::conversation_id(&env.app.db, &m.id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('t-live',?,?,'web','in_flight','ok',?)")
            .bind(&conv).bind(&run).bind(db::now()).execute(&env.app.db).await.unwrap();
        m.id
    }

    /// 這個管理者的對話裡所有「承諾帳本提醒」訊息（不論佇列或已送出）。
    async fn notices(app: &Arc<App>, bot: &str) -> Vec<String> {
        let conv = db::conversation_id(&app.db, bot).await.unwrap();
        sqlx::query_scalar::<_, String>("SELECT m.content FROM messages m JOIN turns t ON t.id=m.turn_id WHERE t.conversation_id=? AND m.content LIKE '承諾帳本提醒%' ORDER BY t.created_at")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 「不知道」的項目（進行中、有下一步、到期時間＝現在）：提醒帶上下一步，管理者下一輪照著複查。
    #[tokio::test]
    async fn a_not_sure_item_reminds_the_manager_with_its_recheck_step() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mgr = manager(&env).await;
        let note = "使用者不確定是否完成：請複查完成度並附驗證證據（commit、測試、截圖或實測結果）後再交件";
        let item = am_base::ledger::commit_item(&app.db, &env.project_id, "user", None, &am_base::ledger::LedgerSplitInput { title: "修 CI".into(), status: Some("following".into()), owner_bot_id: Some(mgr.clone()), next_step: note.into(), due_at: Some(db::now()), ..Default::default() }).await.unwrap();
        sweep(app).await;
        let got = notices(app, &mgr).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(&format!("L{}", item.code_no)) && got[0].contains(&format!("下一步：{note}")), "{}", got[0]);
    }

    /// 沒有承辦人的事項：管理者收到一則提醒；同一版同一階段再掃不會再發；版本變了才再提醒。
    #[tokio::test]
    async fn an_orphan_item_reminds_the_manager_once_per_version() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mgr = manager(&env).await;
        let item = am_base::ledger::commit_item(&app.db, &env.project_id, "user", None, &am_base::ledger::LedgerSplitInput { title: "追蹤 CI".into(), status: Some("following".into()), ..Default::default() }).await.unwrap();

        sweep(app).await;
        let got = notices(app, &mgr).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(&format!("L{}", item.code_no)) && got[0].contains("沒有承辦人"), "{}", got[0]);

        sweep(app).await;
        assert_eq!(notices(app, &mgr).await.len(), 1, "同一版同一階段不重發");

        // 版本變了，但上一則還在佇列裡沒處理：不疊第二則（無重複的未處理提醒）。
        sqlx::query("UPDATE ledger_items SET version = version + 1 WHERE id=?").bind(&item.id).execute(&app.db).await.unwrap();
        sweep(app).await;
        assert_eq!(notices(app, &mgr).await.len(), 1, "上一則還沒處理，不疊");

        // 上一則處理完（回合結束）後，新版本才再提醒。
        let conv = db::conversation_id(&app.db, &mgr).await.unwrap();
        // 佇列裡的提醒依序跑完（queued → in_flight → completed，狀態機不允許直接跳）。
        sqlx::query("UPDATE turns SET status='completed' WHERE id='t-live'").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='in_flight' WHERE conversation_id=? AND status='queued'").bind(&conv).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE conversation_id=? AND status='in_flight'").bind(&conv).execute(&app.db).await.unwrap();
        // 管理者又忙起來（有進行中的回合）：新版本的提醒排進佇列。
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&mgr).fetch_one(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('t-live2',?,?,'web','in_flight','ok',?)")
            .bind(&conv).bind(&run).bind(db::now()).execute(&app.db).await.unwrap();
        sweep(app).await;
        assert_eq!(notices(app, &mgr).await.len(), 2, "處理完、版本變了才再提醒");
    }
    /// 副手名下的到期事項：剛到期只提醒副手；逾期超過 30 分鐘升級給頂層管理者（絕不退回副手本人）。
    #[tokio::test]
    async fn deputy_due_item_reminds_the_deputy_then_escalates_to_the_manager_after_thirty_minutes() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mgr = manager(&env).await;
        let dep = crate::testing::claude_bot(app, &env.project_id, "lg-dep").await;
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&dep.id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&dep.id).bind(&env.project_id).execute(&app.db).await.unwrap();
        let drun = crate::testing::fake_run(app, &dep.id).await;
        let dconv = db::conversation_id(&app.db, &dep.id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('t-dep-live',?,?,'web','in_flight','ok',?)").bind(&dconv).bind(&drun).bind(db::now()).execute(&app.db).await.unwrap();
        let ago = |m: i64| (chrono::Utc::now() - chrono::Duration::minutes(m)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let item = am_base::ledger::commit_item(&app.db, &env.project_id, "user", None, &am_base::ledger::LedgerSplitInput { title: "副手的到期事".into(), status: Some("following".into()), owner_bot_id: Some(dep.id.clone()), due_at: Some(ago(5)), ..Default::default() }).await.unwrap();

        sweep(app).await;
        assert_eq!(notices(app, &dep.id).await.len(), 1, "剛到期：只提醒承辦的副手");
        assert!(notices(app, &mgr).await.is_empty(), "還沒逾期 30 分鐘，不升級");

        // 逾期 35 分鐘（版本推進，才算新的一次）。
        sqlx::query("UPDATE ledger_items SET due_at=?, version=version+1 WHERE id=?").bind(ago(35)).bind(&item.id).execute(&app.db).await.unwrap();
        sweep(app).await;
        assert_eq!(notices(app, &mgr).await.len(), 1, "逾期超過 30 分鐘升級給頂層管理者");
        assert_eq!(notices(app, &dep.id).await.len(), 1, "升級後副手不再收到第二則");
    }

    /// 副手自己名下的逾期事項：目標一定是頂層管理者，即使管理者忙、副手在代班也不退回副手本人。
    #[tokio::test]
    async fn an_escalated_item_never_bounces_back_to_the_deputy_who_owns_it() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mgr = manager(&env).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'").bind(&mgr).execute(&app.db).await.unwrap();
        let dep = crate::testing::claude_bot(app, &env.project_id, "lg-dep2").await;
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&dep.id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&dep.id).bind(&env.project_id).execute(&app.db).await.unwrap();
        crate::testing::fake_run(app, &dep.id).await;
        let ago = (chrono::Utc::now() - chrono::Duration::minutes(40)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        am_base::ledger::commit_item(&app.db, &env.project_id, "user", None, &am_base::ledger::LedgerSplitInput { title: "副手逾期".into(), status: Some("following".into()), owner_bot_id: Some(dep.id.clone()), due_at: Some(ago), ..Default::default() }).await.unwrap();
        let r = am_base::ledger::due_reminders(&app.db, chrono::Utc::now()).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(target_for(app, &r[0]).await.as_deref(), Some(mgr.as_str()), "逾期升級：目標是頂層管理者，不是副手");
    }
}
