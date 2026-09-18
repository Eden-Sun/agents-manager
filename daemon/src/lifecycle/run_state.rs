//! `runs.state` 的轉移：寫不進去就不能當作發生了（#135／#145／#146）。
//!
//! run 的狀態是生命週期的權威：prompt 准入、UI、restart／reconcile 都照它做事。以前各處是
//! `let _ = UPDATE runs SET state=… WHERE id=?`：吞掉寫入錯誤、不看來源狀態。DB 暫時寫不進去時，
//! 外面已經做了不可逆的事（收 in-flight、撤佇列、關 pane、回「啟動成功」），DB 卻還記著舊狀態。
//!
//! [`transition`] 是唯一的寫法，三種結果分開：
//! - `Ok(Moved::Applied)`：轉過去了，後續的副作用才有權做。
//! - `Ok(Moved::Lost)`：CAS 輸了——別的路徑（多半是不拿 bot 鎖的 pane-exit 事件）已經先把這顆 run 帶走，
//!   收尾歸那條路，這裡不重做。
//! - `Err(_)`：DB 寫不進去。狀態沒變；不做後續不可逆的動作，錯誤往上傳。外面的副作用已經發生的，
//!   用 [`schedule_settle`] 排重試，讓狀態照證據收斂，不留一顆永遠卡住的 run。

use super::*;

/// active run 的三個狀態（`db::active_run` 與 active-run 唯一索引的定義）。
pub(crate) const LIVE: &[&str] = &["starting", "running", "stopping"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moved {
    Applied,
    Lost,
}

/// CAS：`from` 其中之一 → `to`。`agent_status` 有給就一起寫；`to` 是終態時補 `ended_at`（已經有就留著）。
pub(crate) async fn transition(
    db: &sqlx::SqlitePool,
    run_id: &str,
    from: &[&str],
    to: &str,
    agent_status: Option<&str>,
) -> Result<Moved, sqlx::Error> {
    debug_assert!(!from.is_empty());
    let terminal = matches!(to, "stopped" | "exited");
    let sql = format!(
        "UPDATE runs SET state = ?, agent_status = COALESCE(?, agent_status),
                ended_at = CASE WHEN ? THEN COALESCE(ended_at, ?) ELSE ended_at END
          WHERE id = ? AND state IN ({})",
        vec!["?"; from.len()].join(",")
    );
    let mut q = sqlx::query(&sql).bind(to).bind(agent_status).bind(terminal).bind(db::now()).bind(run_id);
    for s in from {
        q = q.bind(*s);
    }
    let r = q.execute(db).await?;
    Ok(if r.rows_affected() == 0 { Moved::Lost } else { Moved::Applied })
}

/// 外面的副作用已經做了、狀態卻寫不進去時，之後怎麼把它收斂回來。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settle {
    /// 交給對帳照 herdr 的證據收：agent 還在就 `running`，不在就 `exited`。`stuck` 是寫不過去時的狀態，
    /// run 離開它就算收斂。
    Reconcile { stuck: String },
    /// 使用者的 stop 在外面已經做完（agent 確定沒了）：補記 `stopping → stopped` 與它之後的收尾
    /// （`stop::finish_stop`）。被 pane-exit 事件先收成 `exited` 的也改標回 `stopped`。
    FinishStop,
    /// 停不下來（agent 還活著）：`stopping → running` 放回去。
    BackToRunning,
}

impl Settle {
    /// 這顆 run 現在的狀態是不是還等著這一種重試。
    fn pending(&self, state: &str) -> bool {
        match self {
            Settle::Reconcile { stuck } => state == stuck,
            Settle::FinishStop => matches!(state, "stopping" | "exited"),
            Settle::BackToRunning => state == "stopping",
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Settle::Reconcile { .. } => "reconcile",
            Settle::FinishStop => "finish_stop",
            Settle::BackToRunning => "back_to_running",
        }
    }
}

/// 同一顆 run 的同一種重試只排一條（重試裡的對帳又失敗時不會再疊一條）。只管節流，不是狀態的權威。
fn settling() -> &'static std::sync::Mutex<std::collections::HashSet<(String, &'static str)>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(String, &'static str)>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// 背景重試到這顆 run 離開卡住的狀態。daemon 在那之前重啟的話，開機的對帳照同一份證據收。
/// 測試裡不開背景 task，只記下排了什麼（[`scheduled`]），由測試自己呼叫 [`settle_once`]。
pub(crate) fn schedule_settle(app: &Arc<App>, run_id: &str, how: Settle) {
    tracing::warn!(run = run_id, ?how, "run state is not committed; retrying in the background");
    if cfg!(test) {
        #[cfg(test)]
        test_log().lock().unwrap().push((run_id.to_string(), how));
        return;
    }
    let key = (run_id.to_string(), how.name());
    if !settling().lock().map(|mut s| s.insert(key.clone())).unwrap_or(false) {
        return;
    }
    let app = app.clone();
    let run_id = run_id.to_string();
    tokio::spawn(async move {
        let mut settled = false;
        for secs in [2u64, 5, 15, 30, 60, 120] {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if settle_once(&app, &run_id, &how).await {
                settled = true;
                break;
            }
        }
        if let Ok(mut s) = settling().lock() {
            s.remove(&key);
        }
        if !settled {
            tracing::error!(run = %run_id, ?how, "run state still not settled after retries; the next reconcile (reconnect or restart) takes it");
        }
    });
}

/// 重試一次；回傳這顆 run 是不是已經不再等這一種重試。
pub(crate) async fn settle_once(app: &Arc<App>, run_id: &str, how: &Settle) -> bool {
    let run = match db::run(&app.db, run_id).await {
        Ok(Some(run)) if how.pending(&run.state) => run,
        Ok(_) => return true,
        Err(_) => return false,
    };
    match how {
        Settle::Reconcile { .. } => {
            let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { return false };
            if let Err(e) = crate::reconcile::reconcile_host(app, &host).await {
                tracing::warn!(run = run_id, host, error = %e, "reconcile retry failed");
            }
        }
        Settle::FinishStop => super::finish_stop(app, run_id).await,
        Settle::BackToRunning => {
            let lock = app.bot_lock(&run.bot_id).await;
            let _g = lock.lock().await;
            if transition(&app.db, run_id, &["stopping"], "running", None).await.is_ok() {
                app.emit_bot_status(&run.bot_id).await;
            }
        }
    }
    match db::run(&app.db, run_id).await {
        Ok(Some(r)) => !how.pending(&r.state),
        Ok(None) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
fn test_log() -> &'static std::sync::Mutex<Vec<(String, Settle)>> {
    static L: std::sync::OnceLock<std::sync::Mutex<Vec<(String, Settle)>>> = std::sync::OnceLock::new();
    L.get_or_init(Default::default)
}

/// 測試用：這顆 run 排過哪些重試。
#[cfg(test)]
pub(crate) fn scheduled(run_id: &str) -> Vec<Settle> {
    test_log().lock().unwrap().iter().filter(|(r, _)| r == run_id).map(|(_, s)| s.clone()).collect()
}

/// 測試用的故障注入：之後把 `runs.state` 寫成 `state` 的 UPDATE 一律失敗（像 SQLite 的 I/O／busy 錯誤），
/// 直到 [`accept_run_state`] 拿掉。不影響其他欄位與其他目標狀態的寫入。
#[cfg(test)]
pub(crate) async fn refuse_run_state(app: &Arc<App>, state: &str) {
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_run_state_{state} BEFORE UPDATE OF state ON runs WHEN NEW.state = '{state}'
         BEGIN SELECT RAISE(ABORT, 'injected: cannot write runs.state = {state}'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();
}

/// 測試用：DB 恢復。
#[cfg(test)]
pub(crate) async fn accept_run_state(app: &Arc<App>, state: &str) {
    sqlx::query(&format!("DROP TRIGGER refuse_run_state_{state}")).execute(&app.db).await.unwrap();
}

/// 測試用的故障注入（#156）：之後改這一筆回合 `status` 的 UPDATE 一律失敗，直到 [`accept_turn_close`]。
/// 只擋這一筆：同一顆 bot 其他回合（排著的那些）照常寫得進去。
#[cfg(test)]
pub(crate) async fn refuse_turn_close(app: &Arc<App>, turn_id: &str) {
    sqlx::query(&format!(
        "CREATE TRIGGER refuse_turn_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{turn_id}'
         BEGIN SELECT RAISE(ABORT, 'injected: cannot write turns.status'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();
}

/// 測試用：DB 恢復。
#[cfg(test)]
pub(crate) async fn accept_turn_close(app: &Arc<App>) {
    sqlx::query("DROP TRIGGER refuse_turn_close").execute(&app.db).await.unwrap();
}

/// 測試用：這顆 bot 的對話裡放一筆回合；`in_flight` 的掛在 `run_id` 底下。
#[cfg(test)]
pub(crate) async fn a_turn(app: &Arc<App>, bot_id: &str, run_id: Option<&str>, status: &str) -> String {
    let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
    let id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web',?,'ok','派工',?)")
        .bind(&id)
        .bind(&conv)
        .bind(run_id)
        .bind(status)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    id
}

#[cfg(test)]
pub(crate) async fn turn_status(app: &Arc<App>, turn_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
}

/// 測試用：這筆回合底下補了幾則系統說明（收掉、撤掉時各補一則）。
#[cfg(test)]
pub(crate) async fn system_notes(app: &Arc<App>, turn_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(turn_id).fetch_one(&app.db).await.unwrap()
}

/// 測試用：替這顆 run 的 pane 登記一個 watcher（`unwatch_pane_on_session` 拆的那一份），回傳它的 key。
#[cfg(test)]
pub(crate) async fn watch_run_pane(app: &Arc<App>, run_id: &str) -> (String, String, String) {
    let run = db::run(&app.db, run_id).await.unwrap().unwrap();
    let host = db::bot_host(&app.db, &run.bot_id).await.unwrap();
    let session = app.session_for_run(&run).await.unwrap();
    let key = (host, session, run.pane_id.clone().unwrap());
    app.pane_watchers.lock().await.insert(key.clone(), tokio::spawn(std::future::pending::<()>()));
    key
}

#[cfg(test)]
pub(crate) async fn watched(app: &Arc<App>, key: &(String, String, String)) -> bool {
    app.pane_watchers.lock().await.contains_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn state(app: &Arc<App>, run: &str) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    /// 三種結果分得開：轉過去、CAS 輸了、DB 寫不進去。
    #[tokio::test]
    async fn a_transition_tells_applied_lost_and_a_failed_write_apart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cas").await;
        let run = tt::fake_run(&app, &bot.id).await;

        assert_eq!(transition(&app.db, &run, &["starting"], "running", None).await.unwrap(), Moved::Lost, "它是 running，不是 starting");
        assert_eq!(state(&app, &run).await, "running");

        refuse_run_state(&app, "stopping").await;
        assert!(transition(&app.db, &run, LIVE, "stopping", None).await.is_err());
        assert_eq!(state(&app, &run).await, "running", "寫不進去就是沒轉");
        accept_run_state(&app, "stopping").await;

        assert_eq!(transition(&app.db, &run, LIVE, "stopping", None).await.unwrap(), Moved::Applied);
        assert_eq!(transition(&app.db, &run, &["stopping"], "stopped", None).await.unwrap(), Moved::Applied);
        let ended: Option<String> = sqlx::query_scalar("SELECT ended_at FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert!(ended.is_some(), "終態補上 ended_at");
        assert_eq!(transition(&app.db, &run, LIVE, "running", None).await.unwrap(), Moved::Lost, "終態不會被拉回 active");
    }
}
