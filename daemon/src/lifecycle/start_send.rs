//! bot 沒在跑時按送出：**先落地、再啟動**（issue #122）。
//!
//! 以前 web 把這一則放在瀏覽器記憶體（`queuedSends`），自己按啟動、等 bot 起來再 `POST /prompt`——在那之前
//! daemon 完全不知道這則訊息存在，起 bot 的過程中重整、關分頁、換裝置或啟動失敗，訊息就沒了。
//!
//! 現在 `POST /prompt` 帶 `start_if_stopped`、而這顆 bot 沒有在跑（沒有 active run、或 run 還在 `starting`）時：
//! 1. 在 bot 鎖裡把 turn（`queued`、`awaits_start=1`、要送的 `prompt_text`）＋使用者訊息＋附件綁定寫進**同一個交易**，
//!    commit 之後才回 `delivery: "queued"`。同一個 `client_request_id` 再送一次回同一筆（重整、重送都不會多一則）。
//! 2. 啟動是之後的副作用（[`kick`]）：睡著的走 `idle_sleep::wake` 接回原本的 session，其他的 `start_bot`。不在請求裡等。
//! 3. bot 起來、閒下來，由既有的佇列 flush 送出：CAS claim 保證只送一次，resume／額度／維護窗口的閘門照舊。
//!    瀏覽器不再留一份等著送，所以 WS 幀、重整、重按啟動都不會變成第二次送出。
//!
//! 生命週期：
//! - **啟動失敗、或 run 起來後又結束**：不撤，留在佇列，`start_error` 寫原因——一般「沒有 run 的 queued 一律收掉」
//!   （`queue::revoke_orphaned_queued_turns`）只管 AGM 的派工，不管這種。UI 顯示「沒能啟動」＋重新啟動
//!   （`POST /bots/{id}/start`，起來後照樣由 flush 送）／取消。
//! - **使用者自己按停止**（`stop_bot`）：那是「不要了」，跟起不來不一樣——撤回、寫明理由，不留到下次啟動又送出去。
//! - **取消**：`POST /turns/{id}/withdraw` 撤回（標 failed＋說明），文字由 web 接回輸入框。已經送出去的撤不回來（409）。
//! - **daemon 重啟**：開機對帳完成後（`reconcile::autostart_after_reconcile`），還在等、沒有 run 的再替它啟動一次
//!   （重啟前那次可能根本沒做完、或起不來的原因已經排除）。每次開機最多一次，失敗照樣只記原因。

use super::*;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

enum Accepted {
    /// bot 在跑（或 daemon 本來就不能替它啟動，例如子 agent）：照一般的路走，行為一模一樣。
    Normal,
    /// 收下了（或同一個請求之前就收下了）。帶著「正在啟動」的標記＝要替它啟動：標記在 commit **之前**就拿好，
    /// 撤孤兒的定時掃描（不拿 bot 鎖）才不會在收下與背景啟動之間，把原因寫成「run 已經結束」。
    Queued(PromptOut, Option<Starting>),
}

/// `POST /bots/{id}/prompt` 帶 `start_if_stopped`。
pub async fn prompt_starting(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    let accepted = {
        let lock = app.bot_lock(bot_id).await;
        let _g = lock.lock().await;
        accept_locked(app, bot_id, text, client_request_id, attachment_ids, relay_from).await?
    };
    match accepted {
        Accepted::Normal => prompt_relayed(app, bot_id, text, client_request_id, attachment_ids, relay_from).await,
        Accepted::Queued(out, mark) => {
            if let Some(mark) = mark {
                spawn_start(app, bot_id, mark);
            }
            Ok(out)
        }
    }
}

async fn accept_locked(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<Accepted> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;
    // 冪等：同一個請求再來一次回同一筆。還在等、也沒人在替它啟動的話再踢一次——重送同一個請求就是重試。
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        let waiting = t.status == "queued" && t.awaits_start == 1 && run.is_none();
        let mark = if waiting { Starting::begin(bot_id) } else { None };
        return Ok(Accepted::Queued(super::prompt::answer_for_turn(app, &t).await?, mark));
    }
    // 只有「沒有 run」或「正在起」才收下：在跑的照一般的路；`stopping` 是有人正在停它，收下的會被停止撤回，照一般的路 409。
    let starting_up = run.as_ref().is_none_or(|r| r.state == "starting");
    if !starting_up || (run.is_none() && bot.managed_by == "child") {
        return Ok(Accepted::Normal);
    }
    // 維護窗口開著的時候不收、也不替它啟動（issue #86）：一個字都沒寫，同一個請求之後原樣重送是乾淨的。
    if let Some(refusal) = super::prompt::maintenance_refusal(app, Admission::Gated).await {
        return Err(refusal);
    }
    let files = crate::attach::resolve(app, bot_id, attachment_ids).await.map_err(|e| LcError::Bad(e.to_string()))?;
    let deliver = crate::attach::deliver_text(text, &files);
    let (turn_id, msg_id) = (db::ulid(), db::ulid());
    // 沒有 run 才要替它起（`starting` 那顆已經有人在起）。已經有人在替它起（重複的請求）就不再疊一次。
    let mark = if run.is_none() { Starting::begin(bot_id) } else { None };
    let mut tx = app.db.begin().await.map_err(up)?;
    let queued = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, awaits_start)
         VALUES (?,?,NULL,'web','queued','pending',?,?,?,1)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(client_request_id)
    .bind(db::now())
    .bind(&deliver)
    .execute(&mut *tx)
    .await;
    if let Err(e) = queued {
        // 已經有一筆在排（每個對話最多一筆 queued）：照舊回 409，這一則留在呼叫端。
        tracing::info!(bot = %bot_id, error = %e, "另一筆 prompt 已經在排隊，這次照舊回 409");
        return Err(LcError::conflict("a turn is already queued for this bot", json!({"conversation_id": conv})));
    }
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, created_at) VALUES (?,?,?,'user',?,'web',?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(relay_from)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    // 附件跟訊息同一個交易：要嘛整則（含附件）都收下，要嘛什麼都沒有，不會有一則指著沒綁上的附件在排隊。
    crate::attach::bind_tx(&mut tx, &msg_id, &files).await.map_err(|e| LcError::Bad(e.to_string()))?;
    tx.commit().await.map_err(up)?;
    super::prompt::emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, run = ?run.as_ref().map(|r| &r.id), "bot 沒在跑：先收下這一則，再替它啟動");
    let out = PromptOut { turn_id, message_id: msg_id, delivery: "queued".into(), send_now: None };
    Ok(Accepted::Queued(out, mark))
}

/// 正在替哪幾顆 bot 啟動（只在記憶體）：這段時間沒有 run 是暫時的，撤孤兒那一支不要把原因寫成「沒能啟動」。
fn starting() -> &'static Mutex<HashSet<String>> {
    static M: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

struct Starting(String);

impl Starting {
    /// 已經有人在替它啟動就回 `None`：同一顆不疊兩次啟動。
    fn begin(bot_id: &str) -> Option<Self> {
        starting().lock().ok()?.insert(bot_id.to_string()).then(|| Starting(bot_id.to_string()))
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        if let Ok(mut m) = starting().lock() {
            m.remove(&self.0);
        }
    }
}

fn in_progress(bot_id: &str) -> bool {
    starting().lock().map(|m| m.contains(bot_id)).unwrap_or(false)
}

/// 背景替它啟動（已經有人在起就不疊）。
pub(crate) fn kick(app: &Arc<App>, bot_id: &str) {
    if let Some(mark) = Starting::begin(bot_id) {
        spawn_start(app, bot_id, mark);
    }
}

/// 測試裡不做：背景的 RPC 會跟測試自己推的狀態機搶（同 `schedule_flush_queued`），測試直接呼叫 [`start_for_waiting`]。
fn spawn_start(app: &Arc<App>, bot_id: &str, mark: Starting) {
    if cfg!(test) {
        return;
    }
    let (app, id) = (app.clone(), bot_id.to_string());
    tokio::spawn(async move { start_with(&app, &id, mark).await });
}

/// 替還在等的那一則啟動 bot（已經有人在起就不疊）。正式路徑都在背景（[`kick`]），測試直接等它做完。
#[cfg(test)]
pub(crate) async fn start_for_waiting(app: &Arc<App>, bot_id: &str) {
    if let Some(mark) = Starting::begin(bot_id) {
        start_with(app, bot_id, mark).await;
    }
}

/// 成功 → 叫醒 flush（起來、閒下來就送）；失敗 → 留在佇列，`start_error` 寫原因。`mark` 在啟動做完時放掉。
async fn start_with(app: &Arc<App>, bot_id: &str, mark: Starting) {
    set_start_error(app, bot_id, None, false).await;
    let res = match crate::supervisor::idle_sleep::wake(app, bot_id, "有一則訊息等著它起來送").await {
        Ok(true) => Started::Yes,
        Ok(false) => match start_bot(app, bot_id).await {
            Ok(_) => Started::Yes,
            // 別人（使用者按了啟動、AGM）剛好先起了：一樣是「起來了」。
            Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("active run already exists") => Started::Yes,
            // agent 起來了、只差 `running` 沒記下（#152）：不是沒能啟動。
            Err(LcError::Uncommitted(v)) => match v.get("run_id").and_then(|r| r.as_str()) {
                Some(run_id) => Started::NotRecorded(run_id.to_string()),
                None => Started::Yes,
            },
            Err(e) => Started::Failed(failure_text(&e)),
        },
        // 叫醒（`--resume`）的錯誤是字串，分不出是哪一種：這顆已經有一個 `starting` 的 run 留著，就是 agent 起來了、
        // 狀態沒記下（真的起不來時 start 會把 run 收成 `exited`）。
        Err(e) => match db::active_run(&app.db, bot_id).await {
            Ok(Some(r)) if r.state == "starting" => Started::NotRecorded(r.id),
            _ => Started::Failed(format!("{e:#}")),
        },
    };
    drop(mark);
    match res {
        Started::Yes => {
            tracing::info!(bot = %bot_id, "替等著送的訊息把 bot 起來了：起來、閒下來就由佇列送出");
            schedule_flush_queued(app, bot_id);
        }
        Started::NotRecorded(run_id) => flush_once_running(app, bot_id, &run_id),
        Started::Failed(why) => {
            tracing::warn!(bot = %bot_id, error = %why, "替等著送的訊息啟動 bot 失敗：訊息留在佇列");
            set_start_error(app, bot_id, Some(&why), false).await;
        }
    }
}

enum Started {
    Yes,
    /// agent 起來了，run 停在 `starting`（`running` 寫不進 DB，對帳重試會收斂）。
    NotRecorded(String),
    Failed(String),
}

/// 替它起的 agent 已經起來、`running` 卻沒寫進 DB（`Uncommitted`，#152）：對帳重試會照 herdr 的證據把 run 收成
/// `running`，但那條路不會叫 flush——排著的這一則就沒人送。這裡在背景等它收斂：`running` 就叫醒 flush；run 不在了
/// （收成 `exited`）就停，撤孤兒那條路會記原因。測試裡不開背景 task，只記下排了誰（[`watching_for_running`]），
/// 由測試自己呼叫 [`flush_if_running`]。
fn flush_once_running(app: &Arc<App>, bot_id: &str, run_id: &str) {
    tracing::warn!(bot = %bot_id, run = %run_id, "agent 起來了、running 還沒記下：等對帳收斂後再叫 flush");
    if cfg!(test) {
        #[cfg(test)]
        watch_log().lock().unwrap().push(run_id.to_string());
        return;
    }
    let (app, bot_id, run_id) = (app.clone(), bot_id.to_string(), run_id.to_string());
    tokio::spawn(async move {
        for secs in [2u64, 5, 15, 30, 60, 120, 300] {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if flush_if_running(&app, &bot_id, &run_id).await {
                return;
            }
        }
        tracing::warn!(bot = %bot_id, run = %run_id, "run 還沒收成 running；之後的 idle 邊或重啟接回會叫 flush");
    });
}

/// 看一次：`true`＝不必再等（收成 `running` 已叫 flush，或 run 已經不在）。
pub(crate) async fn flush_if_running(app: &Arc<App>, bot_id: &str, run_id: &str) -> bool {
    match db::run(&app.db, run_id).await {
        Ok(Some(r)) if r.state == "running" => {
            schedule_flush_queued(app, bot_id);
            true
        }
        Ok(Some(r)) if r.state == "starting" => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
fn watch_log() -> &'static Mutex<Vec<String>> {
    static L: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// 測試用：有沒有替這顆 run 排「收成 running 就叫 flush」。
#[cfg(test)]
pub(crate) fn watching_for_running(run_id: &str) -> bool {
    watch_log().lock().unwrap().iter().any(|r| r == run_id)
}

/// 啟動失敗講人話：409／400 的 body 優先用 `hint`／`message`，再退回 `reason`。
fn failure_text(e: &LcError) -> String {
    match e {
        LcError::NotFound(what) => format!("找不到 {what}"),
        LcError::Bad(m) | LcError::Upstream(m) => m.clone(),
        LcError::Conflict(v) | LcError::BadValue(v) | LcError::Unprocessable(v) | LcError::Forbidden(v) | LcError::Unavailable(v) | LcError::Uncommitted(v) => ["hint", "message", "reason"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
            .unwrap_or("啟動被拒絕")
            .to_string(),
    }
}

/// 這顆 bot 還在等它起來的那幾則（`queued`＋`awaits_start`）。
async fn waiting_turns(app: &Arc<App>, bot_id: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND t.status = 'queued' AND t.awaits_start = 1",
    )
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default()
}

/// `keep_first`：已經有原因就不蓋（撤孤兒的定時掃描每分鐘都會來，不能把「找不到 claude」蓋成「它的 run 已經結束」）。
async fn set_start_error(app: &Arc<App>, bot_id: &str, why: Option<&str>, keep_first: bool) {
    for id in waiting_turns(app, bot_id).await {
        let changed = if keep_first {
            sqlx::query("UPDATE turns SET start_error=? WHERE id=? AND status='queued' AND start_error IS NULL")
                .bind(why)
                .bind(&id)
                .execute(&app.db)
                .await
        } else {
            sqlx::query("UPDATE turns SET start_error=? WHERE id=? AND status='queued' AND start_error IS NOT ?")
                .bind(why)
                .bind(&id)
                .bind(why)
                .execute(&app.db)
                .await
        };
        if matches!(changed, Ok(r) if r.rows_affected() > 0) {
            emit_turn(app, &id).await;
        }
    }
}

/// 撤孤兒那一支（`queue::revoke_orphaned_queued_turns`）遇到這種：不撤，只記原因。正在替它啟動（run 還沒建出來）
/// 那一段不算，免得把「起到一半」講成「沒能啟動」。
pub(crate) async fn note_run_gone(app: &Arc<App>, bot_id: &str, why: &str) {
    if in_progress(bot_id) {
        return;
    }
    set_start_error(app, bot_id, Some(why), true).await;
}

/// 使用者自己按停止（`stop_bot`）：還在等它起來的那幾則一併撤回，不留到下次啟動又送出去。
/// 重啟（換身分、`?resume=native`）的停不算——那段沒有 run 是暫時的（`restart_hold`）。
pub(crate) async fn withdraw_on_stop(app: &Arc<App>, bot_id: &str) {
    if super::restart_hold::in_progress(bot_id) {
        return;
    }
    let why = "bot 已被停止：這一則是 bot 沒在跑時送的，還在等它起來，沒有送出，一併撤銷，不會再送。";
    for id in waiting_turns(app, bot_id).await {
        if let Err(e) = revoke_queued_turn(app, &id, why).await {
            tracing::error!(bot = %bot_id, turn = %id, error = %e, "could not withdraw a starting send on stop");
        }
    }
}

/// 使用者取消（`POST /turns/{id}/withdraw`）：還在等 bot 起來的那一則撤回、寫明理由，不送。
///
/// 只撤這一種。已經被佇列領走（`in_flight`，字已經打進去了）或本來就不是這種的一律 409——不能拿「放棄回合」
/// 頂替：那會把已經送出的回合收成失敗，web 又把文字放回輸入框，使用者再按一次就送了兩次。
pub async fn withdraw_turn(app: &Arc<App>, turn_id: &str) -> LcResult<()> {
    let bot_id: String = sqlx::query_scalar("SELECT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let lock = app.bot_lock(&bot_id).await;
    let _g = lock.lock().await;
    let (status, awaits_start): (String, i64) = sqlx::query_as("SELECT status, awaits_start FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_one(&app.db)
        .await
        .map_err(up)?;
    let why = "使用者取消了這一則：它是 bot 沒在跑時送的，還在等 bot 起來，沒有送出，不會再送。";
    if status != "queued" || awaits_start != 1 || !revoke_queued_turn(app, turn_id, why).await.map_err(up)? {
        return Err(LcError::conflict("turn is not waiting for its bot to start", json!({"turn_id": turn_id, "status": status})));
    }
    Ok(())
}

/// 開機（那台主機對帳完成、autostart 那一步）：還在等 bot 起來、沒有 run 的，再替它啟動一次。
/// 重啟前那次啟動可能根本沒做完。回傳踢了幾顆。
pub(crate) async fn resume_after_boot(app: &Arc<App>, host: &str) -> usize {
    let bots: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id JOIN bots b ON b.id = c.bot_id
          WHERE t.status = 'queued' AND t.awaits_start = 1 AND b.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = c.bot_id AND r.state IN ('starting','running','stopping'))",
    )
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    let mut kicked = 0;
    for bot_id in bots {
        if db::bot_host(&app.db, &bot_id).await.unwrap_or_else(|_| LOCAL_HOST.to_string()) != host {
            continue;
        }
        tracing::info!(host, bot = %bot_id, "開機：有一則訊息等著這顆起來，替它啟動");
        kick(app, &bot_id);
        kicked += 1;
    }
    kicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    const TEXT: &str = "起來後幫我跑一次測試";

    async fn turn_of(app: &Arc<App>, crid: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE client_request_id=?").bind(crid).fetch_one(&app.db).await.unwrap()
    }

    async fn count(app: &Arc<App>, sql: &str, bind: &str) -> i64 {
        sqlx::query_scalar(sql).bind(bind).fetch_one(&app.db).await.unwrap()
    }

    fn started(e: &tt::Env) -> usize {
        e.herdr.methods().iter().filter(|m| *m == "agent.start").count()
    }

    /// 送出的那一刻就在 DB 裡（turn＋訊息同一個交易），啟動是之後的事——在那之前 daemon 重啟、定時掃描跑過，都還在。
    /// 同一個請求再送一次（重整後重送、網路重試）回同一筆，不會多一則。
    #[tokio::test]
    async fn a_send_to_a_stopped_bot_is_on_disk_before_any_start_and_survives_a_restart() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "stopped").await;
        let out = prompt_starting(&app, &bot.id, TEXT, "crid-1", &[], None).await.unwrap();
        assert_eq!(out.delivery, "queued");
        let t = turn_of(&app, "crid-1").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.awaits_start, t.prompt_text.as_deref()), ("queued", None, 1, Some(TEXT)));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='user'", &t.id).await, 1);
        assert_eq!(started(&e), 0, "請求裡不等啟動（啟動是背景的副作用）");

        let again = prompt_starting(&app, &bot.id, TEXT, "crid-1", &[], None).await.unwrap();
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (t.id.as_str(), "queued"), "同一個請求回同一筆");
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE role='user' AND content=?", TEXT).await, 1, "沒有多一則");

        // daemon 重啟：記憶體全空，定時的撤孤兒掃描照常跑——這一則不是孤兒。
        let fresh = tt::restart_app(&e).await;
        crate::lifecycle::revoke_all_orphaned_queued_turns(&fresh).await;
        let t = turn_of(&fresh, "crid-1").await;
        assert_eq!((t.status.as_str(), t.awaits_start), ("queued", 1), "還在佇列");
        assert_eq!(resume_after_boot(&fresh, LOCAL_HOST).await, 1, "開機對帳完替它再啟動一次");
        assert_eq!(resume_after_boot(&fresh, "elsewhere").await, 0, "別台主機的不歸這一輪");
    }

    /// 啟動失敗不丟訊息：留在佇列、原因寫在 turn 上（UI 顯示「沒能啟動」＋重新啟動／取消）。每分鐘的撤孤兒掃描
    /// 不把它撤掉，也不把真正的原因蓋成「run 已經結束」。取消＝撤回、寫明理由；已經撤掉的再取消回 409。
    #[tokio::test]
    async fn a_failed_start_keeps_the_send_with_its_reason_until_the_user_cancels_it() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "no-identity").await;
        // 這台沒有這個身分：`start_bot` 在碰任何 pane 之前就 409。
        sqlx::query("UPDATE bots SET identity='ghost' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        prompt_starting(&app, &bot.id, TEXT, "crid-fail", &[], None).await.unwrap();
        {
            // 正在替它起（run 還沒建出來）：不拿 bot 鎖的定時掃描剛好跑過，不能把原因寫成「run 已經結束」。
            let _mark = Starting::begin(&bot.id).expect("沒有別人在起");
            crate::lifecycle::revoke_all_orphaned_queued_turns(&app).await;
            assert_eq!(turn_of(&app, "crid-fail").await.start_error, None, "起到一半不算沒能啟動");
        }
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!(t.status, "queued", "起不來也不丟");
        let why = t.start_error.clone().expect("原因寫在 turn 上");
        assert!(why.contains("ghost"), "{why}");
        assert!(!in_progress(&bot.id));

        crate::lifecycle::revoke_all_orphaned_queued_turns(&app).await;
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", Some(why.as_str())), "掃描不撤、也不蓋掉原因");

        // 取消：撤回、寫明理由，這個對話的排隊名額空出來。
        withdraw_turn(&app, &t.id).await.unwrap();
        let t = turn_of(&app, "crid-fail").await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE '%使用者取消%'", &t.id).await, 1);
        assert!(matches!(withdraw_turn(&app, &t.id).await, Err(LcError::Conflict(_))), "撤掉的不能再取消一次");
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 0, "撤掉的不再替它啟動");
    }

    /// 取消只撤「還在等 bot 起來」的那一則：已經被佇列領走（字打進去了）、或 AGM 排的派工，一律 409、原樣不動——
    /// 拿「放棄回合」頂替的話，web 會把已經送出的文字放回輸入框，再按一次就送兩次。
    #[tokio::test]
    async fn withdraw_refuses_anything_but_a_send_still_waiting_for_its_bot() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "withdraw").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-w", &[], None).await.unwrap();
        let t = turn_of(&app, "crid-w").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE turns SET status='in_flight', run_id=? WHERE id=?").bind(&run).bind(&t.id).execute(&app.db).await.unwrap();
        assert!(matches!(withdraw_turn(&app, &t.id).await, Err(LcError::Conflict(_))), "已經送出去的撤不回來");
        assert_eq!(turn_of(&app, "crid-w").await.status, "in_flight", "原樣不動");

        let other = tt::claude_bot(&app, &e.project_id, "agm-queued").await;
        let conv = db::conversation_id(&app.db, &other.id).await.unwrap();
        let agm = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&agm)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        assert!(matches!(withdraw_turn(&app, &agm).await, Err(LcError::Conflict(_))), "AGM 的派工不歸這裡撤");
        assert!(matches!(withdraw_turn(&app, "no-such-turn").await, Err(LcError::NotFound(_))));
    }

    /// run 起來之後又結束（pane 不見、agent 退出）不是使用者不要了：一樣留著、記原因。使用者自己按停止才撤。
    #[tokio::test]
    async fn only_the_users_own_stop_withdraws_a_starting_send() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "run-ended").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='starting' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let out = prompt_starting(&app, &bot.id, TEXT, "crid-run", &[], None).await.unwrap();
        assert_eq!(out.delivery, "queued", "run 還在 starting：一樣先收下（有人在起它，不必再踢一次）");

        crate::lifecycle::mark_run_exited(&app, &run, "pane 不見了").await;
        let t = turn_of(&app, "crid-run").await;
        assert_eq!(t.status, "queued", "run 結束不撤");
        assert!(t.start_error.as_deref().is_some_and(|w| w.contains("pane 不見了")), "{:?}", t.start_error);

        let run2 = tt::fake_run(&app, &bot.id).await;
        crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap();
        let t = turn_of(&app, "crid-run").await;
        assert_eq!(t.status, "failed", "使用者自己按停止：撤回");
        assert_eq!(count(&app, "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE 'bot 已被停止%'", &t.id).await, 1);
        let _ = run2;
    }

    /// 在跑的 bot：旗標沒有作用，走一般的送出（有 run、沒有 `awaits_start`）。附件跟訊息同一個交易綁上；
    /// 認不得的附件 400，什麼都沒寫。
    #[tokio::test]
    async fn a_running_bot_takes_the_normal_path_and_attachments_bind_with_the_send() {
        let e = tt::env().await;
        let app = e.app.clone();
        let running = tt::claude_bot(&app, &e.project_id, "running").await;
        tt::fake_run(&app, &running.id).await;
        // 假的 run 沒有 transcript：一般那條路照它自己的規則回 409 `transcript_not_ready`——重點是沒有被「收下再啟動」。
        let normal = prompt_starting(&app, &running.id, "照常送", "crid-running", &[], None).await;
        assert!(
            matches!(&normal, Err(LcError::Conflict(v)) if v["reason"] == "transcript_not_ready"),
            "在跑的 bot 照一般的路：{normal:?}"
        );
        assert_eq!(count(&app, "SELECT COUNT(*) FROM turns WHERE awaits_start=1 AND client_request_id=?", "crid-running").await, 0);

        let stopped = tt::claude_bot(&app, &e.project_id, "stopped-attach").await;
        let att = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'shot.png','image/png',1,'/tmp/shot.png','/tmp/shot.png','local',?)",
        )
        .bind(&att)
        .bind(&stopped.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let bad = prompt_starting(&app, &stopped.id, TEXT, "crid-bad", &["nope".to_string()], None).await;
        assert!(matches!(bad, Err(LcError::Bad(_))));
        assert_eq!(count(&app, "SELECT COUNT(*) FROM turns WHERE client_request_id=?", "crid-bad").await, 0, "認不得的附件：什麼都沒寫");

        let out = prompt_starting(&app, &stopped.id, TEXT, "crid-att", std::slice::from_ref(&att), None).await.unwrap();
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?").bind(&att).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(out.message_id.as_str()), "附件綁在這一則上");
        let t = turn_of(&app, "crid-att").await;
        assert!(t.prompt_text.as_deref().is_some_and(|p| p.contains("/tmp/shot.png")), "起來後送的字帶著附件路徑");
    }

    /// issue #152（start_send 那一半）：替等著送的訊息啟動時 agent 真的起來了，`running` 卻寫不進 DB（`Uncommitted`）——
    /// 那不是「沒能啟動」：不寫 `start_error`。對帳重試把 run 收成 `running` 不會叫 flush，所以這裡要自己等到它收斂再叫；
    /// 收斂之後那一則送出去，只送一次。會真的 `start_bot`，要有 claude（遠端編譯主機沒有）。
    #[tokio::test]
    async fn a_start_that_came_up_but_could_not_record_running_still_sends_once_it_settles() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "uncommitted").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-u", &[], None).await.unwrap();
        super::super::run_state::refuse_run_state(&app, "running").await;
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-u").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", None), "agent 起來了：不是沒能啟動");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 停在 starting，沒被收掉");
        assert_eq!(run.state, "starting");
        assert!(watching_for_running(&run.id), "排了「收成 running 就叫 flush」");
        assert!(!flush_if_running(&app, &bot.id, &run.id).await, "還在 starting：繼續等");

        // DB 恢復，對帳照 herdr 的證據收成 running。
        super::super::run_state::accept_run_state(&app, "running").await;
        let stuck = super::super::run_state::Settle::Reconcile { stuck: "starting".into() };
        assert!(super::super::run_state::settle_once(&app, &run.id, &stuck).await);
        assert!(flush_if_running(&app, &bot.id, &run.id).await, "收成 running：叫 flush，不再等");
        let pane = db::run(&app.db, &run.id).await.unwrap().unwrap().pane_id.unwrap();
        e.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(&app.db, &run.id).await.unwrap();
        for _ in 0..3 {
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&app, &bot.id).await.unwrap();
        }
        let t = turn_of(&app, "crid-u").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.start_error.as_deref()), ("in_flight", Some(run.id.as_str()), None));
        let sent = e.herdr.pane(&pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(TEXT)).count());
        assert_eq!(sent, 1, "送出，只送一次");
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }

    /// 同上，走睡著的 bot（`idle_sleep::wake` 用 `--resume` 叫醒，回的是字串錯誤）：留下一個 `starting` 的 run 就是
    /// agent 起來了、狀態沒記下，一樣不寫 `start_error`、等收斂再叫 flush。會真的 `start_bot`，要有 claude。
    #[tokio::test]
    async fn a_sleeping_bot_woken_for_a_send_that_could_not_record_running_is_not_a_failed_start() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "asleep-uncommitted").await;
        sqlx::query("INSERT INTO bot_sleeps (bot_id, idle_minutes, reason, slept_at) VALUES (?, 30, 'idle', ?)")
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        prompt_starting(&app, &bot.id, TEXT, "crid-sleep", &[], None).await.unwrap();
        super::super::run_state::refuse_run_state(&app, "running").await;
        start_for_waiting(&app, &bot.id).await;
        assert_eq!(turn_of(&app, "crid-sleep").await.start_error, None, "叫醒時 agent 起來了：不是沒能啟動");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 停在 starting");
        assert!(watching_for_running(&run.id));
        super::super::run_state::accept_run_state(&app, "running").await;
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }

    /// 整條：沒在跑 → 收下 → daemon 重啟（啟動還沒發生）→ 開機替它啟動 → 起來、閒下來 → 佇列送出，只送一次；
    /// 多叫醒幾次、同一個請求重送、再踢一次啟動，都不會再送。會真的 `start_bot`，要有 claude（遠端編譯主機沒有）。
    #[tokio::test]
    async fn a_send_to_a_stopped_bot_goes_out_exactly_once_after_the_daemon_starts_it() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "boot-start").await;
        prompt_starting(&app, &bot.id, TEXT, "crid-e2e", &[], None).await.unwrap();

        let app = tt::restart_app(&e).await;
        assert_eq!(resume_after_boot(&app, LOCAL_HOST).await, 1);
        start_for_waiting(&app, &bot.id).await;
        let t = turn_of(&app, "crid-e2e").await;
        assert_eq!((t.status.as_str(), t.start_error.as_deref()), ("queued", None), "起來了，還沒送（等閒下來）");
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("run 起來了");
        let pane = run.pane_id.clone().unwrap();
        e.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(&app.db, &run.id).await.unwrap();
        let sent = || e.herdr.pane(&pane).map_or(0, |p| p.transcript.iter().filter(|l| l.contains(TEXT)).count());

        for _ in 0..3 {
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&app, &bot.id).await.unwrap();
        }
        let t = turn_of(&app, "crid-e2e").await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("in_flight", Some(run.id.as_str())));
        assert_eq!(sent(), 1, "送出，只送一次");

        let again = prompt_starting(&app, &bot.id, TEXT, "crid-e2e", &[], None).await.unwrap();
        assert_eq!(again.turn_id, t.id, "同一個請求重送：回同一筆，不再開一則");
        start_for_waiting(&app, &bot.id).await;
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert_eq!(sent(), 1, "再踢一次啟動、再叫醒 flush：不會再送");
        forget_queue_retry_timer(&bot.id);
        stop_bot(&app, &bot.id).await.unwrap();
    }
}
