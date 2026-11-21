//! bot 沒在跑時按送出：**先落地、再啟動**（issue #122）。
//!
//! 以前 web 把這一則放在瀏覽器記憶體（`queuedSends`），自己按啟動、等 bot 起來再 `POST /prompt`——在那之前
//! daemon 完全不知道這則訊息存在，起 bot 的過程中重整、關分頁、換裝置或啟動失敗，訊息就沒了。
//!
//! 現在 `POST /prompt` 帶 `start_if_stopped`、而這顆 bot 沒有在跑（沒有 active run、或 run 還在 `starting`）時：
//! 1. 在 bot 鎖裡把 turn（`queued`、`awaits_start=1`、要送的 `prompt_text`）＋使用者訊息＋附件綁定寫進**同一個交易**，
//!    commit 之後才回 `delivery: "queued"`。同一個 `client_request_id` 再送一次回同一筆（重整、重送都不會多一則）。
//! 2. 啟動是之後的副作用（[`kick`]）：睡著的走 `idle_sleep::wake` 接回原本的 session，分享用 bot 以 `resume_native` 接回，
//!    其他的 `start_bot`。不在請求裡等。
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

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;
use super::send_now::ports::AttachTxPort;
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
#[cfg(all(test, feature = "daemon-test-harness"))]
pub async fn prompt_starting(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
) -> LcResult<PromptOut> {
    prompt_starting_or_queue(app, bot_id, text, client_request_id, attachment_ids, relay, false).await
}

/// [`prompt_starting`]，另外帶著 `queue_if_busy`：bot 已經在跑（照一般的路）時，旗標不能掉，不然忙碌中的 bot 回 409
/// 而不是落地排隊（#733）。bot 沒在跑／正在起的那條路不看它。
pub async fn prompt_starting_or_queue(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
) -> LcResult<PromptOut> {
    prompt_starting_or_queue_authorized(app, bot_id, text, client_request_id, attachment_ids, relay, queue_if_busy, None).await
}

/// Share-portal variant: the token is rechecked while holding the same bot lock used by rotation and deletion.
pub async fn prompt_starting_or_queue_with_share_token(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
    token: &str,
) -> LcResult<PromptOut> {
    prompt_starting_or_queue_authorized(app, bot_id, text, client_request_id, attachment_ids, relay, queue_if_busy, Some(token)).await
}

async fn prompt_starting_or_queue_authorized(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
    share_token: Option<&str>,
) -> LcResult<PromptOut> {
    let accepted = {
        let _g = crate::lifecycle::s6_ports::StartServices::lock_bot_for_start(app, bot_id).await;
        match db::bot(app.db(), bot_id).await {
            Ok(Some(bot)) if !crate::config::is_api_kind(&bot.kind) => {},
            Ok(Some(_)) => return Err(LcError::Bad("api_native_prompt_unsupported".into())),
            _ => return Err(LcError::Upstream("api_start_send_unconfirmed".into())),
        }
        if let Some(token) = share_token {
            let valid = app.resolve_share_token(token).await.map_err(|e| LcError::Upstream(e.to_string()))?;
            if valid.as_deref() != Some(bot_id) {
                return Err(LcError::NotFound("bot".into()));
            }
            app.touch_share(bot_id).await;
        }
        accept_locked(app, bot_id, text, client_request_id, attachment_ids, relay).await?
    };
    match accepted {
        Accepted::Normal if queue_if_busy => {
            match share_token {
                Some(token) => super::prompt::prompt_from_share_with_token(app, bot_id, text, client_request_id, attachment_ids, relay, true, token).await,
                None => prompt_from_api_queue_if_busy(app, bot_id, text, client_request_id, attachment_ids, relay, false, None).await,
            }
        }
        Accepted::Normal => match share_token {
            Some(token) => super::prompt::prompt_from_share_with_token(app, bot_id, text, client_request_id, attachment_ids, relay, false, token).await,
            None => prompt_from_api(app, bot_id, text, client_request_id, attachment_ids, relay, false, None).await,
        },
        Accepted::Queued(out, mark) => {
            if let Some(mark) = mark {
                spawn_start(app, bot_id, mark);
            }
            Ok(out)
        }
    }
}

async fn accept_locked(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
) -> LcResult<Accepted> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    // #708：移交出去的專案不收 prompt、不排隊、也不替它啟動（同 `prompt_inner`）。放在冪等檢查之前：讀不到是 502、不寫任何列。
    super::send_now::ports::HandoffSendRepo::refuse_handed_off(app, bot_id).await?;
    let bot = db::bot(app.db(), bot_id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(app.db(), bot_id).await.map_err(up)?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?;
    // 冪等：同一個請求再來一次回同一筆。還在等、也沒人在替它啟動的話再踢一次——重送同一個請求就是重試。
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
    {
        super::prompt::check_same_text(app, &t, text).await?;
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
    let files = app.resolve_attachments(bot_id, attachment_ids).await.map_err(|e| LcError::Bad(e.to_string()))?;
    // 同 `prompt_inner`：起來後打進 pane 的是清過的字（#205），泡泡留原文。
    let deliver = super::pane_text::for_pane(&bot.kind, &crate::attach::deliver_text(text, &files));
    if super::pane_text::nothing_left(text, &deliver) {
        return Err(LcError::Bad("the prompt is empty once invisible characters and terminal escapes are removed".into()));
    }
    let (turn_id, msg_id) = (db::ulid(), db::ulid());
    // 沒有 run 才要替它起（`starting` 那顆已經有人在起）。已經有人在替它起（重複的請求）就不再疊一次。
    let mark = if run.is_none() { Starting::begin(bot_id) } else { None };
    let mut tx = app.db().begin().await.map_err(up)?;
    let queued = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, queue_class, created_at, prompt_text, awaits_start)
         VALUES (?,?,NULL,'web','queued','pending',?,?,?, ?,1)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(client_request_id)
    .bind(relay.queue_class.as_str())
    .bind(db::now())
    .bind(&deliver)
    .execute(&mut *tx)
    .await;
    if let Err(e) = queued {
        return Err(super::prompt::queue_insert_error(bot_id, &conv, e));
    }
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, relay_auth, created_at) VALUES (?,?,?,'user',?,'web',?,?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(relay.auth)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    // 附件跟訊息同一個交易：要嘛整則（含附件）都收下，要嘛什麼都沒有，不會有一則指著沒綁上的附件在排隊。
    tx.bind_attachments_tx(&msg_id, &files).await.map_err(|e| LcError::Bad(e.to_string()))?;
    tx.commit().await.map_err(up)?;
    if relay.from.is_none() {
        if let Err(e) = am_base::ledger::ingest_if_management_message(app.db(), bot_id, &msg_id, Some(&turn_id), text).await {
            tracing::warn!(bot = %bot_id, error = %e, "帳本收件失敗（訊息已經收下）");
        }
    }
    super::prompt::publish_prompt_message(app, bot_id, &msg_id).await;
    crate::lifecycle::s6_ports::QueueServices::turn_changed(app, &turn_id).await;
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
pub fn kick(app: &impl LcHost, bot_id: &str) {
    if let Some(mark) = Starting::begin(bot_id) {
        spawn_start(app, bot_id, mark);
    }
}

/// 測試裡不做：背景的 RPC 會跟測試自己推的狀態機搶（同 `schedule_flush_queued`），測試直接呼叫 [`start_for_waiting`]。
fn spawn_start(app: &impl LcHost, bot_id: &str, mark: Starting) {
    if cfg!(test) {
        return;
    }
    let (app, id) = ((*app).clone(), bot_id.to_string());
    tokio::spawn(async move { start_with(&app, &id, mark).await });
}

/// 替還在等的那一則啟動 bot（已經有人在起就不疊）。正式路徑都在背景（[`kick`]），測試直接等它做完。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub async fn start_for_waiting(app: &Arc<App>, bot_id: &str) {
    if let Some(mark) = Starting::begin(bot_id) {
        start_with(app, bot_id, mark).await;
    }
}

/// 成功 → 叫醒 flush（起來、閒下來就送）；失敗 → 留在佇列，`start_error` 寫原因。`mark` 在啟動做完時放掉。
async fn start_with(app: &impl LcHost, bot_id: &str, mark: Starting) {
    // The kick may outlive a withdrawal/stop. Recheck at actual start admission under the
    // same lock as cancellation, then use the locked wake/start surfaces below.
    let bot_guard = crate::lifecycle::s6_ports::StartServices::lock_bot_for_start(app, bot_id).await;
    if !matches!(db::bot(app.db(), bot_id).await, Ok(Some(bot)) if !crate::config::is_api_kind(&bot.kind)) { return; }
    let waiting = sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id
          WHERE c.bot_id=? AND t.status='queued' AND t.awaits_start=1 AND {})",
        super::queue::scheduled_turn_allowed_sql("t.id")
    ))
    .bind(bot_id)
    .fetch_one(app.db())
    .await;
    match waiting {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(bot = bot_id, error = %error, "cannot verify a waiting prompt; refusing queued start");
            return;
        }
    }
    set_start_error(app, bot_id, None, false).await;
    let res = match app.idle_sleep_wake_locked(bot_id, "有一則訊息等著它起來送").await {
        Ok(true) => Started::Yes,
        Ok(false) => match start_stopped(app, bot_id).await {
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
        Err(e) => match db::active_run(app.db(), bot_id).await {
            Ok(Some(r)) if r.state == "starting" => Started::NotRecorded(r.id),
            _ => Started::Failed(format!("{e:#}")),
        },
    };
    drop(bot_guard);
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

/// 沒睡著、就是停了的那顆。分享用 bot（SPEC §20）是 daemon 重啟、主機重開、pane 被關之後停的——外部 end user 不知道
/// 它停過，要接回原本那段對話再送（`resume_native`；接不回照舊開新對話，訊息不能卡住）。讀不到是不是分享用 bot 也接回：
/// 對一般 bot 只差在「先試著接回」，對分享用 bot 開新對話就是把對方的脈絡丟掉。其他 bot 照舊開新對話。
async fn start_stopped(app: &impl LcHost, bot_id: &str) -> LcResult<String> {
    let opts = if matches!(app.is_share_bot(bot_id).await, Ok(false)) {
        StartOpts::default()
    } else {
        StartOpts { resume_native: true, ..Default::default() }
    };
    start_bot_locked_with(app, bot_id, opts).await
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
/// 由測試自己呼叫 [`flush_if_running`]。重啟（`restart_bot_with`／子 agent 原地重啟）遇到同一種 `Uncommitted` 也走這裡（#165）。
pub fn flush_once_running(app: &impl LcHost, bot_id: &str, run_id: &str) {
    tracing::warn!(bot = %bot_id, run = %run_id, "agent 起來了、running 還沒記下：等對帳收斂後再叫 flush");
    if cfg!(test) {
        #[cfg(all(test, feature = "daemon-test-harness"))]
        watch_log().lock().unwrap().push(run_id.to_string());
        return;
    }
    let (app, bot_id, run_id) = ((*app).clone(), bot_id.to_string(), run_id.to_string());
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
pub async fn flush_if_running(app: &impl LcHost, bot_id: &str, run_id: &str) -> bool {
    match db::run(app.db(), run_id).await {
        Ok(Some(r)) if r.state == "running" => {
            schedule_flush_queued(app, bot_id);
            true
        }
        Ok(Some(r)) if r.state == "starting" => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
fn watch_log() -> &'static Mutex<Vec<String>> {
    static L: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// 測試用：有沒有替這顆 run 排「收成 running 就叫 flush」。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub fn watching_for_running(run_id: &str) -> bool {
    watch_log().lock().unwrap().iter().any(|r| r == run_id)
}

/// 啟動失敗講人話：409／400 的 body 優先用 `hint`／`message`，再退回 `reason`。
fn failure_text(e: &LcError) -> String {
    match e {
        LcError::NotFound(what) => format!("找不到 {what}"),
        LcError::Bad(m) | LcError::Upstream(m) => m.clone(),
        LcError::Conflict(v) | LcError::BadValue(v) | LcError::NotFoundValue(v) | LcError::Unprocessable(v) | LcError::Forbidden(v) | LcError::Unavailable(v) | LcError::Uncommitted(v) => ["hint", "message", "reason"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
            .unwrap_or("啟動被拒絕")
            .to_string(),
    }
}

/// 這顆 bot 還在等它起來的那幾則（`queued`＋`awaits_start`）。
async fn waiting_turns(app: &impl crate::capabilities::Db, bot_id: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND t.status = 'queued' AND t.awaits_start = 1",
    )
    .bind(bot_id)
    .fetch_all(app.db())
    .await
    .unwrap_or_default()
}

/// `keep_first`：已經有原因就不蓋（撤孤兒的定時掃描每分鐘都會來，不能把「找不到 claude」蓋成「它的 run 已經結束」）。
async fn set_start_error(app: &impl LcHost, bot_id: &str, why: Option<&str>, keep_first: bool) {
    for id in waiting_turns(app, bot_id).await {
        let changed = if keep_first {
            sqlx::query("UPDATE turns SET start_error=? WHERE id=? AND status='queued' AND start_error IS NULL")
                .bind(why)
                .bind(&id)
                .execute(app.db())
                .await
        } else {
            sqlx::query("UPDATE turns SET start_error=? WHERE id=? AND status='queued' AND start_error IS NOT ?")
                .bind(why)
                .bind(&id)
                .bind(why)
                .execute(app.db())
                .await
        };
        if matches!(changed, Ok(r) if r.rows_affected() > 0) {
            crate::lifecycle::s6_ports::QueueServices::turn_changed(app, &id).await;
        }
    }
}

/// 撤孤兒那一支（`queue::revoke_orphaned_queued_turns`）遇到這種：不撤，只記原因。正在替它啟動（run 還沒建出來）
/// 那一段不算，免得把「起到一半」講成「沒能啟動」。
pub async fn note_run_gone(app: &impl LcHost, bot_id: &str, why: &str) {
    if in_progress(bot_id) {
        return;
    }
    set_start_error(app, bot_id, Some(why), true).await;
}

/// 使用者按停止時撤回的說明。
const STOP_WHY: &str = "bot 已被停止：這一則是 bot 沒在跑時送的，還在等它起來，沒有送出，一併撤銷，不會再送。";

/// 使用者自己按停止：還在等它起來的那幾則一併撤回，不留到下次啟動又送出去。呼叫端是 `stop` 記 `stopping` 的**同一個交易**
/// （#199）：要嘛「正在停」與撤回都成立，要嘛都不成立（stop 一步都不做）。以前兩步分開、撤不掉只記在記憶體（#159 的欠帳），
/// 補上之前 daemon 重啟，開機就照舊替它啟動、把使用者已經不要的那一則送出去。
/// 重啟（換身份、`?resume=native`）的停不撤——那段沒有 run 是暫時的（`restart_hold`），由呼叫端判斷。
/// 回傳撤掉的那幾則，commit 之後逐一 [`announce_revoked`]。
pub async fn withdraw_waiting_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    bot_id: &str,
) -> anyhow::Result<Vec<(String, super::Revoked)>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND t.status = 'queued' AND t.awaits_start = 1",
    )
    .bind(bot_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut withdrawn = Vec::new();
    for id in ids {
        if let Some(r) = revoke_queued_turn_tx(tx, &id, STOP_WHY).await? {
            withdrawn.push((id, r));
        }
    }
    Ok(withdrawn)
}

/// 使用者取消（`POST /turns/{id}/withdraw`）：還在等 bot 起來的那一則、或還在排的 daemon 自動通知（#562）撤回、寫明理由，不送。
///
/// 只撤這三種。已經被佇列領走（`in_flight`，字已經打進去了）或本來就不是這種的一律 409——不能拿「放棄回合」
/// 頂替：那會把已經送出的回合收成失敗，web 又把文字放回輸入框，使用者再按一次就送了兩次。
pub struct WithdrawnPrompt {
    pub text: String,
    pub attachments: Vec<String>,
}

/// 派工的 bot 能撤回的只有它自己派出、經 token 驗證（`relay_unverified=0`）、還在佇列裡（`queued`）的那一則。
/// 已送出（in_flight／done）的、別人派的、自稱沒驗過的都不行。
pub async fn sender_may_withdraw(app: &impl LcHost, turn_id: &str, sender: &str) -> LcResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM turns t JOIN messages m ON m.turn_id = t.id AND m.role = 'user' \
         WHERE t.id = ? AND t.status = 'queued' AND m.relay_from = ? AND m.relay_unverified = 0",
    )
    .bind(turn_id)
    .bind(sender)
    .fetch_one(app.db())
    .await
    .map_err(up)?;
    Ok(n > 0)
}

/// 派工者自己撤回（見 [`sender_may_withdraw`]）。撤掉的這一則不會再送；原文不還給任何輸入框（不是使用者的訊息）。
pub async fn withdraw_turn_by_sender(app: &impl LcHost, turn_id: &str, sender: &str) -> LcResult<()> {
    let bot_id: String = sqlx::query_scalar("SELECT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id=?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let _g = crate::lifecycle::s6_ports::StartServices::lock_bot_for_start(app, &bot_id).await;
    if !sender_may_withdraw(app, turn_id, sender).await? {
        return Err(LcError::conflict("turn is not a queued relay from this bot", json!({"turn_id": turn_id})));
    }
    if !revoke_queued_turn(app, turn_id, "派工的 bot 撤回了這一則：還沒送出，不會再送。").await.map_err(up)? {
        return Err(LcError::conflict("turn is not waiting in the queue", json!({"turn_id": turn_id})));
    }
    Ok(())
}

pub async fn withdraw_turn(app: &impl LcHost, turn_id: &str) -> LcResult<WithdrawnPrompt> {
    let bot_id: String = sqlx::query_scalar("SELECT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id=?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let _g = crate::lifecycle::s6_ports::StartServices::lock_bot_for_start(app, &bot_id).await;
    let (status, awaits_start, awaits_idle, origin, queue_class): (String, i64, i64, String, String) = sqlx::query_as(
        "SELECT status, awaits_start, awaits_idle, origin, queue_class FROM turns WHERE id=?",
    )
    .bind(turn_id)
    .fetch_one(app.db())
    .await
    .map_err(up)?;
    // 還在排的 daemon 自動通知也可以撤（#562）：它擋在佇列頭時，使用者要能讓自己的訊息先走。
    let why = if awaits_start == 1 {
        "使用者取消了這一則：它是 bot 沒在跑時送的，還在等 bot 起來，沒有送出，不會再送。"
    } else if awaits_idle == 1 && origin == "web" {
        "使用者取消了這一則：bot 忙碌時先排進佇列，還沒送出，不會再送。"
    } else if queue_class == super::prompt::QueueClass::DaemonNotice.as_str() {
        super::daemon_notice::WITHDRAWN_WHY
    } else {
        ""
    };
    if status != "queued" || why.is_empty() {
        return Err(LcError::conflict("turn is not waiting for its bot to start", json!({"turn_id": turn_id, "status": status})));
    }
    let restores_user_prompt = awaits_start == 1 || (awaits_idle == 1 && origin == "web");
    let (text, attachments) = if restores_user_prompt {
        let message: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT content, attachments_json FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at, rowid LIMIT 1",
        )
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?;
        let (text, encoded_attachments) = message.ok_or_else(|| LcError::Upstream("queued user turn has no user message".into()))?;
        let attachments = encoded_attachments
            .map(|json| serde_json::from_str::<Vec<crate::attach::Attachment>>(&json).map(|items| items.into_iter().map(|item| item.id).collect()))
            .transpose()
            .map_err(up)?
            .unwrap_or_default();
        (text, attachments)
    } else {
        (String::new(), Vec::new())
    };
    if !revoke_queued_turn(app, turn_id, why).await.map_err(up)? {
        return Err(LcError::conflict("turn is not waiting for its bot to start", json!({"turn_id": turn_id, "status": status})));
    }
    Ok(WithdrawnPrompt { text, attachments })
}

/// 開機（那台主機對帳完成、autostart 那一步）：還在等 bot 起來、沒有 run 的，再替它啟動一次。
/// 重啟前那次啟動可能根本沒做完。回傳踢了幾顆。
///
/// 讀不到就延後，不猜（#198）：以前清單讀不到當成「沒有」、主機讀不到當成本機——本機開機時把還沒對帳的遠端 bot
/// 也起一顆（同一顆 bot 兩個 agent），真的在本機的那顆反而可能永遠沒人起。現在讀不到的 [`RESUME_RETRY`] 之後再看一次。
pub async fn resume_after_boot(app: &impl LcHost, host: &str) -> usize {
    resume_waiting(app, host, None).await
}

/// 開機替等著的 bot 起來時，讀不到清單或主機，多久之後再看。
const RESUME_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// `only`：重看時只看上一次讀不到主機的那幾顆。
async fn resume_waiting(app: &impl LcHost, host: &str, only: Option<HashSet<String>>) -> usize {
    let bots: Vec<String> = match sqlx::query_scalar(&format!(
        "SELECT DISTINCT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id JOIN bots b ON b.id = c.bot_id
          WHERE t.status = 'queued' AND t.awaits_start = 1 AND b.deleted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = c.bot_id AND r.state IN ('starting','running','stopping'))
            AND {}", super::queue::scheduled_turn_allowed_sql("t.id")),
    )
    .fetch_all(app.db())
    .await
    {
        Ok(bots) => bots,
        Err(e) => {
            tracing::warn!(host, error = %e, "開機：讀不到等著起來的 bot，稍後再看");
            retry_resume(app, host, only);
            return 0;
        }
    };
    let mut kicked = 0;
    let mut unknown = HashSet::new();
    for bot_id in bots.into_iter().filter(|b| only.as_ref().is_none_or(|o| o.contains(b))) {
        match db::bot_host(app.db(), &bot_id).await {
            Ok(h) if h == host => {}
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(host, bot = %bot_id, error = %e, "開機：讀不到這顆 bot 在哪台主機，先不替它起、稍後再看");
                unknown.insert(bot_id);
                continue;
            }
        }
        match db::bot(app.db(), &bot_id).await {
            Ok(Some(bot)) if !crate::config::is_api_kind(&bot.kind) => {},
            Ok(Some(_)) => continue,
            _ => { unknown.insert(bot_id); continue; }
        }
        tracing::info!(host, bot = %bot_id, "開機：有一則訊息等著這顆起來，替它啟動");
        kick(app, &bot_id);
        kicked += 1;
    }
    if !unknown.is_empty() {
        retry_resume(app, host, Some(unknown));
    }
    kicked
}

fn retry_resume(app: &impl LcHost, host: &str, only: Option<HashSet<String>>) {
    if cfg!(test) {
        return;
    }
    let (app, host) = ((*app).clone(), host.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(RESUME_RETRY).await;
        resume_waiting(&app, &host, only).await;
    });
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub fn in_progress(bot_id: &str) -> bool { super::in_progress(bot_id) }
    /// `Starting` 本身維持私有；daemon 測試只要「拿到就占著、放掉就釋放」這個行為。
    pub struct StartingGuard(#[allow(dead_code)] super::Starting);
    impl StartingGuard {
        pub fn begin(bot_id: &str) -> Option<Self> { super::Starting::begin(bot_id).map(StartingGuard) }
    }
}
