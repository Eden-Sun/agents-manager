//! `--resume` 接回之後、驗證之前，prompt 不准進去（issue #92）。
//!
//! 換身分（撞額度 → 換帳號）或閒置叫醒用 `--resume <session>` 把原本那段對話接回來。CLI 接不接得回，
//! 要等它自己回報 session 才知道：claude 起來時的 `SessionStart` hook 帶的 session 跟要接的一樣就是接回了
//! （`hookrecv::consume_resume_session` 記成 `verified`）；不一樣就是 CLI 默默開了新對話（`mismatch`，
//! 聊天室裡會有一則 `context_lost` 說明）。以前兩件事都在，**中間沒有閘門**：run 一變成 running＋idle，
//! 排隊的派工就被 flush 進去——`SessionStart` 若還在路上（遠端 hook 走 spool、daemon 剛好在重啟、hook 背景
//! worker 還拿不到 bot 鎖），那一則就送進一段還沒確認是不是原本那段的對話裡。
//!
//! 這裡是那道閘門，規則只住在這一個檔案：
//! - **誰要等**：claude、有注入 hook（`inject_hooks`，沒有 hook 就沒有驗證來源）、這個 run 是 `resume_native`
//!   起的（`runs.resume_session_id` 還在），而且還沒有結論（`runs.resume_outcome` 是 NULL）。
//!   codex／grok 本來就要等第一個回合結束才回報 session（`hookrecv` 的既有規則），不在這裡等——等下去
//!   只會死結：沒有回合就沒有回報，沒有回報就不給回合。
//! - **等到什麼時候**：`SessionStart` 來了就放行（對上或對不上都是結論，對不上的那則說明會先進聊天室）；
//!   最多等 [`VERIFY_WINDOW`]，從 `runs.started_at` 算。到期是**刻意的退路**：記 `resume_outcome='unverified'`、
//!   在聊天室留一則看得見的說明、然後放行——hook 壞掉的 bot 不能因此永遠收不到訊息，但也不能假裝驗過了。
//!   之後才到的 `SessionStart` 照樣會被比對，對不上一樣會留 `context_lost`。
//! - **擋的是什麼**：排隊的 flush（`queue::flush_queued_locked`，不花重試額度，掛 timer 到期再來）與直接送入
//!   （`prompt::prompt_inner`：AGM 派工排進佇列；使用者回 409 `resume_unverified` 帶 `retry_after_s`）。
//! - **重啟**：到期時間就是 `runs.started_at + VERIFY_WINDOW`，存在 DB 裡，不另外養 timer 狀態。行程內的 timer
//!   沒了也不用另外接：`rearm_queue_retries` 開機時本來就把每一筆 queued 叫醒一次，flush 走到這裡會照原本的
//!   到期時間重新掛上（SPEC「到期動作不靠行程內的 timer 當唯一真相」）。

use super::*;

/// 從 run 開始到 `SessionStart` 進到 daemon 最多等多久。本機通常幾秒；遠端 hook 寫在那台的 spool，
/// 要等 30 秒一輪的掃描（SPEC §11.4.4）撈回來，再加上 CLI 自己的啟動時間（`start_inner` 最多等 60 秒 ready）。
pub(crate) const VERIFY_WINDOW: Duration = Duration::from_secs(120);

/// 這一刻該不該讓 prompt 進去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Gate {
    /// 沒在等：沒要求接回、已經有結論、或這顆 bot 沒有驗證來源。
    Open,
    /// 還在等 CLI 回報 session；最多再等 `left`。
    Waiting { expected: String, left: Duration },
    /// 等滿了還沒回報：要走刻意的退路（[`check`] 會記下來並留說明，再當成 `Open`）。
    Expired { expected: String },
}

/// 純規則：所有輸入都由呼叫端給，測得到。
pub(crate) fn decide(bot: &db::Bot, run: &db::Run, now: chrono::DateTime<chrono::Utc>) -> Gate {
    if bot.kind != "claude" || bot.inject_hooks == 0 {
        return Gate::Open;
    }
    let Some(expected) = run.resume_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Gate::Open;
    };
    if run.resume_outcome.is_some() {
        return Gate::Open;
    }
    match remaining(&run.started_at, now) {
        Some(left) => Gate::Waiting { expected: expected.to_string(), left },
        None => Gate::Expired { expected: expected.to_string() },
    }
}

/// 還要等多久；`None` ＝已經到期。`started_at` 讀不懂就當到期：寧可走退路（會留說明），也不要永遠卡住。
fn remaining(started_at: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?.with_timezone(&chrono::Utc);
    let deadline = started + chrono::Duration::from_std(VERIFY_WINDOW).ok()?;
    (deadline - now).to_std().ok().filter(|d| !d.is_zero())
}

/// 呼叫端用這支：`Expired` 在這裡落地（CAS 寫 `resume_outcome='unverified'`＋系統訊息，同一個交易、只寫一次），
/// 然後當成 `Open` 回去。呼叫端持 bot 鎖。
pub(crate) async fn check(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> Gate {
    match decide(bot, run, chrono::Utc::now()) {
        Gate::Expired { expected } => {
            if let Err(e) = give_up_waiting(app, bot, run, conv, &expected).await {
                // 寫不進去也要放行：卡在這裡比少一則說明更糟，下一次呼叫會再試著寫。
                tracing::warn!(bot = %bot.name, run = %run.id, error = %e, "could not record the unverified resume");
            }
            Gate::Open
        }
        other => other,
    }
}

async fn give_up_waiting(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str, expected: &str) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    let marked = sqlx::query(
        "UPDATE runs SET resume_outcome = 'unverified'
          WHERE id = ? AND resume_outcome IS NULL AND resume_session_id IS NOT NULL",
    )
    .bind(&run.id)
    .execute(&mut *tx)
    .await?;
    // 另一條路剛好先寫了結論（hook 在這一瞬間到了、或另一次 check）：那一條負責說明，這裡什麼都不加。
    if marked.rows_affected() == 0 {
        return Ok(());
    }
    let note = format!(
        "⚠️ 用 `--resume` 接回原本的對話（session `{expected}`）之後，{} 秒內沒有收到 claude 回報的 session，\
         確認不了接回的是不是同一段。排隊的訊息照常送出；之後如果回報的 session 對不上，會再另外提醒。",
        VERIFY_WINDOW.as_secs()
    );
    let msg = insert_message_tx(&mut tx, conv, None, "system", &note, "system", false, None).await?;
    tx.commit().await?;
    emit_message_added(app, &bot.id, msg).await;
    tracing::warn!(bot = %bot.name, run = %run.id, expected, "resume was never confirmed; released the prompt gate as a deliberate fallback");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }

    async fn bot_and_run(kind: &str, inject_hooks: i64, resume: Option<&str>, started_at: &str) -> (tt::Env, db::Bot, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "gate").await;
        sqlx::query("UPDATE bots SET kind = ?, inject_hooks = ? WHERE id = ?")
            .bind(kind)
            .bind(inject_hooks)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?,?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(started_at)
        .bind(resume)
        .execute(&env.app.db)
        .await
        .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        (env, bot, run)
    }

    /// 誰要等、等多久、什麼時候算到期——整張規則表。
    #[tokio::test]
    async fn only_an_unconfirmed_claude_resume_with_hooks_waits_and_only_for_the_window() {
        let t0 = "2026-09-18T08:00:00.000Z";
        let (_e, bot, run) = bot_and_run("claude", 1, Some("s-1"), t0).await;
        let s = Duration::from_secs;
        assert_eq!(decide(&bot, &run, at("2026-09-18T08:00:30Z")), Gate::Waiting { expected: "s-1".into(), left: s(90) });
        assert_eq!(decide(&bot, &run, at("2026-09-18T08:02:00Z")), Gate::Expired { expected: "s-1".into() }, "剛好到期");
        assert_eq!(decide(&bot, &run, at("2026-09-18T09:00:00Z")), Gate::Expired { expected: "s-1".into() });

        let mut decided = run.clone();
        decided.resume_outcome = Some("verified".into());
        assert_eq!(decide(&bot, &decided, at("2026-09-18T08:00:30Z")), Gate::Open, "已經有結論");
        decided.resume_outcome = Some("mismatch".into());
        assert_eq!(decide(&bot, &decided, at("2026-09-18T08:00:30Z")), Gate::Open, "對不上也是結論：context_lost 已經說過了");

        let mut fresh = run.clone();
        fresh.resume_session_id = None;
        assert_eq!(decide(&bot, &fresh, at("2026-09-18T08:00:30Z")), Gate::Open, "沒要求接回");
        fresh.resume_session_id = Some("  ".into());
        assert_eq!(decide(&bot, &fresh, at("2026-09-18T08:00:30Z")), Gate::Open);

        let mut hookless = bot.clone();
        hookless.inject_hooks = 0;
        assert_eq!(decide(&hookless, &run, at("2026-09-18T08:00:30Z")), Gate::Open, "沒有 hook 就沒有驗證來源");
        for kind in ["codex", "grok"] {
            let mut other = bot.clone();
            other.kind = kind.into();
            assert_eq!(decide(&other, &run, at("2026-09-18T08:00:30Z")), Gate::Open, "{kind} 要等第一個回合才回報，不在這裡等");
        }
        let mut garbled = run.clone();
        garbled.started_at = "not a time".into();
        assert_eq!(decide(&bot, &garbled, at("2026-09-18T08:00:30Z")), Gate::Expired { expected: "s-1".into() }, "讀不懂就走退路，不要永遠卡住");
    }

    /// 到期是刻意的退路：記 `unverified`、留一則看得見的說明、放行——而且只做一次。
    #[tokio::test]
    async fn an_expired_wait_is_recorded_once_with_a_visible_note_and_then_released() {
        let long_ago = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(600));
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-late"), &long_ago).await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        assert_eq!(check(&e.app, &bot, &run, &conv).await, Gate::Open);
        assert_eq!(check(&e.app, &bot, &run, &conv).await, Gate::Open, "拿舊的 run 快照再問一次");
        let (outcome, sid): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT resume_outcome, resume_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("unverified"));
        assert_eq!(sid.as_deref(), Some("s-late"), "之後才到的 SessionStart 還要能比對：要接的 session 不清掉");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&conv)
            .fetch_all(&e.app.db)
            .await
            .unwrap();
        assert_eq!(notes.len(), 1, "只說一次：{notes:?}");
        assert!(notes[0].contains("s-late") && notes[0].contains("確認不了"), "{}", notes[0]);
    }

    /// 還在窗口內：只回 `Waiting`，什麼都不寫。
    #[tokio::test]
    async fn a_wait_inside_the_window_writes_nothing() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-now"), &db::now()).await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        assert!(matches!(check(&e.app, &bot, &run, &conv).await, Gate::Waiting { ref expected, .. } if expected == "s-now"));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(outcome, None);
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conv).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(notes, 0);
    }

    async fn queue_one(app: &Arc<App>, bot_id: &str, text: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(text)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        turn_id
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// claude 起來時的 `SessionStart`，由這個 run 自己的行程送來。
    async fn session_start(app: &Arc<App>, bot_id: &str, run_id: &str, session: &str) {
        crate::hookrecv::process(
            app,
            &crate::hookrecv::HookBody {
                bot_id: bot_id.to_string(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "SessionStart", "session_id": session, "source": "resume"}),
                received_at: None,
                truncated: false,
                run_id: Some(run_id.to_string()),
            },
        )
        .await
        .unwrap();
    }

    /// flush 走到了閘門後面（claim 了，或送不出去被放回佇列並花了一次重試——這裡沒有真的 pane）。
    fn went_past_the_gate(t: &db::Turn) -> bool {
        t.status != "queued" || t.flush_retries > 0
    }

    /// 整條走一遍：還沒驗證時排隊的 prompt 不動（不 claim、不算重試、一個字都不打、掛好 timer），
    /// `SessionStart` 對上之後才往下送。
    #[tokio::test]
    async fn a_queued_prompt_waits_until_the_resumed_session_is_confirmed() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "換身分之後的第一句").await;
        forget_queue_retry_timer(&bot.id);

        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = turn(&app, &queued).await;
        assert_eq!((t.status.as_str(), t.flush_retries, t.run_id.clone()), ("queued", 0, None), "還沒驗證：不 claim、不花重試");
        assert!(!e.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&bot.id), "閒著的 bot 沒有邊會叫醒它：要掛 timer 到期再來");
        assert!(queue_retry_timer_left(&bot.id).unwrap() <= VERIFY_WINDOW);

        session_start(&app, &bot.id, &run.id, "s-keep").await;
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("verified"));
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await), "驗證過了就照常送");
        forget_queue_retry_timer(&bot.id);
    }

    /// CLI 默默開了新對話（`mismatch`）也是結論：先在聊天室說清楚，再放行——刻意的退路，不是永遠卡住。
    #[tokio::test]
    async fn a_mismatched_resume_says_so_first_and_then_releases_the_queue() {
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "ping").await;
        session_start(&app, &bot.id, &run.id, "s-brand-new").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("接不回")), "{notes:?}");
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("mismatch"));
        forget_queue_retry_timer(&bot.id);
    }

    /// 遠端主機：hook 不走 HTTP，是那台的 `hook.sh` 寫進 spool、daemon drain 回來再走同一支 `process`（SPEC §11.4）。
    /// 用真的 `hook.sh`（本機 `/bin/sh` 冒充遠端）產生 `SessionStart` 那一行：遠端 run 一樣要等驗證、那一行帶的
    /// run id 讓它被認成這一代、驗證之後一樣放行。
    #[tokio::test]
    async fn a_remote_run_waits_the_same_way_and_its_spooled_session_start_releases_it() {
        use std::io::Write as _;
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-far"), &db::now()).await;
        let app = e.app.clone();
        let far = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/home/u/far', 'far', 'far-box', ?)")
            .bind(&far)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&far).bind(&bot.id).execute(&app.db).await.unwrap();
        assert_eq!(db::bot_host(&app.db, &bot.id).await.unwrap(), "far-box");
        let queued = queue_one(&app, &bot.id, "遠端那顆的派工").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let held = turn(&app, &queued).await;
        assert_eq!((held.status.as_str(), held.flush_retries), ("queued", 0), "遠端 run 一樣要等驗證");

        // 「遠端」那台：真的 hook.sh，pane env 帶這個 run 的 AM_RUN_ID，沒有 herdr（只寫 spool）。
        let home = e.dir.join("far-home");
        std::fs::create_dir_all(&home).unwrap();
        let script = home.join("hook.sh");
        std::fs::write(&script, remote_hook_sh(crate::startup::REMOTE_ROOT)).unwrap();
        let mut ch = std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(["claude", &bot.id, "tok"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &home)
            .env("AM_RUN_ID", &run.id)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(br#"{"hook_event_name":"SessionStart","session_id":"s-far","source":"resume"}"#).unwrap();
        assert!(ch.wait().unwrap().success());
        let spool = home.join(crate::startup::REMOTE_ROOT).join("bots").join(&bot.id).join("hook-spool.jsonl");
        let line = std::fs::read_to_string(&spool).unwrap();
        let body: crate::hookrecv::HookBody = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(body.run_id.as_deref(), Some(run.id.as_str()));

        crate::hookrecv::process(&app, &body).await.unwrap();
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("verified"));
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await), "驗證過了：往下走（這台沒有 herdr，會被放回佇列並花一次重試）");
        forget_queue_retry_timer(&bot.id);
    }

    /// 一直沒回報：等滿就走退路，flush 照常送，聊天室裡有說明。
    #[tokio::test]
    async fn a_flush_after_the_window_takes_the_deliberate_fallback() {
        let long_ago = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(600));
        let (e, bot, run) = bot_and_run("claude", 1, Some("s-silent"), &long_ago).await;
        let app = e.app.clone();
        let queued = queue_one(&app, &bot.id, "ping").await;
        forget_queue_retry_timer(&bot.id);
        flush_queued_locked(&app, &bot.id).await.unwrap();
        assert!(went_past_the_gate(&turn(&app, &queued).await));
        let outcome: Option<String> = sqlx::query_scalar("SELECT resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(outcome.as_deref(), Some("unverified"));
        forget_queue_retry_timer(&bot.id);
    }

    /// 直接送入也要擋：AGM 派工排進佇列等驗證；使用者的 prompt 回可重試的 409，一筆 turn 都不建。
    #[tokio::test]
    async fn a_direct_prompt_before_verification_is_queued_for_agm_and_refused_for_the_user() {
        let (e, bot, _run) = bot_and_run("claude", 1, Some("s-keep"), &db::now()).await;
        let app = e.app.clone();
        forget_queue_retry_timer(&bot.id);

        match prompt(&app, &bot.id, "使用者直接打的", "req-user").await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "resume_unverified", "{v}");
                assert_eq!(v["session_id"], "s-keep");
                assert!(v["retry_after_s"].as_u64().unwrap() >= 1);
            }
            other => panic!("expected 409 resume_unverified, got {other:?}"),
        }
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0, "被擋下的那一則不留任何 turn：同一個 request id 之後原樣重送是乾淨的");

        let out = prompt_relayed_queueable(&app, &bot.id, "AGM 派的工作", "req-agm", Some(crate::agent_relay::DAEMON_SENDER)).await.unwrap();
        assert_eq!(out.delivery, "queued");
        assert_eq!(turn(&app, &out.turn_id).await.status, "queued");
        assert!(!e.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&bot.id), "驗證到期時 flush 會回來");
        forget_queue_retry_timer(&bot.id);
    }

    /// daemon 重啟：行程內的 timer 都沒了。開機的 `rearm_queue_retries` 把排著的那筆叫醒一次，flush 走到閘門
    /// 照 **DB 裡的到期時間**（`started_at` 起算）重新掛 timer——不是從重啟那一刻重新算，也不會就此放行。
    #[tokio::test]
    async fn after_a_daemon_restart_the_wait_resumes_on_the_original_deadline() {
        let started = db::iso_at(chrono::Utc::now() - chrono::Duration::seconds(30));
        let (e, bot, _run) = bot_and_run("claude", 1, Some("s-wait"), &started).await;
        let queued = queue_one(&e.app, &bot.id, "重啟前排進來的").await;
        forget_queue_retry_timer(&bot.id);

        let app = tt::restart_app(&e).await;
        assert!(rearm_queue_retries(&app).await.unwrap() >= 1, "開機接回排著的 prompt");
        assert!(queue_retry_timer_armed(&bot.id));
        forget_queue_retry_timer(&bot.id); // 那個 timer 燒起來做的就是下面這一步
        flush_queued_locked(&app, &bot.id).await.unwrap();
        let t = turn(&app, &queued).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "重啟不等於驗證過了");
        let left = queue_retry_timer_left(&bot.id).expect("閘門重新掛了 timer");
        assert!(left <= VERIFY_WINDOW - Duration::from_secs(29), "照原本的到期時間，不是從重啟重新算：{left:?}");
        assert!(left >= VERIFY_WINDOW - Duration::from_secs(60), "{left:?}");
        forget_queue_retry_timer(&bot.id);
    }
}
