//! codex 帳號安全提醒橫幅顯示中時擋住派送、通知人（#782，#779 後續）。
//!
//! 橫幅（`› 1. Set up security`／`Press a number to choose · …`，辨識見 [`super::codex_inline_banner`]）顯示中時，
//! 輸入框看起來是空的、可以打字，但 codex 會把 prompt 開頭的數字當成「選第 N 項」吃掉，殘字留在框裡卡住之後的派送。
//! 使用者裁示（2026-10-03）：**不要**自動按 Esc 或任何鍵關橫幅；送字前看到就整則不送（可重試的 `NotAttempted`），
//! 並在 bot 的對話裡留一則 system 通知請人處理，同時推一則 `ops_alert` 進 AGM inbox 給巡檢（#789）。
//! 同一個 run 的同一次橫幅只講一次：佇列每次重試都會再擋一次。

use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// 被橫幅擋下時 `NotAttempted` 的 reason（API 回 409 帶這個 reason，佇列照可重試的規則放回去）。
pub(crate) const REASON: &str = "codex_security_banner";

pub(crate) const NOTICE: &str = "codex 畫面上有帳號安全提醒橫幅（`Press a number to choose`）。橫幅開著時打字，開頭的數字會被 codex 當成選項吃掉，\
     所以這則訊息沒有送出（一個字都沒打）；daemon 也不會替你按 Esc 或任何鍵。請到「終端」處理橫幅（選一個選項，或按 Esc 關掉），\
     關掉後排著的訊息會照常重試送出。";

/// 已經通知過、橫幅還沒被看到關掉的 run，以及這個 run 第幾次出現（橫幅關掉再出現要換 event_key）。
struct Seen {
    open: HashSet<String>,
    gen: HashMap<String, u32>,
}

fn seen() -> &'static Mutex<Seen> {
    static OPEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(Seen { open: HashSet::new(), gen: HashMap::new() }))
}

/// 畫面上有沒有「按數字選」的 inline banner。`screen` 可以是 `format: ansi` 讀到的：去掉樣式，codex 的點字動畫粒子
/// （落在空白格上）也抹回空白，不然空白列不空、結構對不上就漏掉。只有資訊、沒有選項的 banner（`esc to dismiss ·
/// type to continue`）不吃數字，不擋。
pub(crate) fn blocks_typing(screen: &str) -> bool {
    let plain = blank_codex_particles(screen);
    let lines: Vec<&str> = plain.lines().collect();
    codex_inline_banner(&lines)
        .is_some_and(|r| lines[r].iter().any(|l| l.trim_start().starts_with("Press a number to choose")))
}

/// 送字前看到橫幅：第一次寫一則通知（`message_added` 事件推給網頁），也推一則 inbox 事件給巡檢（[`alert`]）。
/// 之後同一次橫幅不再講；看到橫幅不在了就忘掉，下次再出現會再講一次。寫不進去只記 log：擋住派送本身不受影響。
pub(crate) async fn observe(app: &Arc<App>, run: &db::Run, shown: bool) {
    if !shown {
        let mut g = seen().lock().unwrap();
        if g.open.remove(&run.id) {
            *g.gen.entry(run.id.clone()).or_default() += 1;
        }
        return;
    }
    let gen = {
        let mut g = seen().lock().unwrap();
        if !g.open.insert(run.id.clone()) {
            return;
        }
        g.gen.get(&run.id).copied().unwrap_or(0)
    };
    tracing::warn!(run = %run.id, bot = %run.bot_id, "codex 帳號安全提醒橫幅擋住派送，等人處理（不自動關）");
    notify_inbox(app, run, gen).await;
    match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conv) => {
            if let Err(e) = insert_message(app, &conv, None, "system", NOTICE, "system", false, None).await {
                tracing::warn!(run = %run.id, error = ?e, "could not post the codex security banner notice");
            }
        }
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not post the codex security banner notice"),
    }
    alert(app, run).await;
}

/// 同一次橫幅推一則 `ops_alert`（`source=daemon`、`reason=codex_security_banner`）進 AGM inbox：巡檢收、叫醒（#789）。
/// 只寫進 bot 對話的話，沒人開著那顆 bot 的頁面就不會知道它整條佇列卡住。
///
/// 「同一次」由 [`observe`] 判斷（只在橫幅剛出現時叫到這裡），event_key 帶 run 與這一次的 id：
/// 消失又出現是新的一次、新的 key；`push_inbox` 的 `INSERT OR IGNORE` 擋重送。daemon 重啟後記憶清空，
/// 還開著的橫幅會再推一則——寧可多一則，不要漏。
async fn alert(app: &Arc<App>, run: &db::Run) {
    let name = match db::bot(&app.db, &run.bot_id).await {
        Ok(Some(b)) => b.name,
        _ => String::new(),
    };
    let subject = if name.is_empty() { run.bot_id.as_str() } else { name.as_str() };
    let key = format!("ops_alert:daemon:{REASON}:{}:{}", run.id, db::ulid());
    let payload = json!({
        "source": "daemon",
        "reason": REASON,
        "subject": subject,
        "bot_id": run.bot_id,
        "bot_name": name,
        "run_id": run.id,
        "detail": format!(
            "codex bot `{name}`（{}）畫面上有帳號安全提醒橫幅（`Press a number to choose`）：橫幅開著時打字，開頭的數字會被當成選項吃掉，daemon 已擋下派送（一個字都沒打）",
            run.bot_id
        ),
        "action": "請人到這顆 bot 的「終端」pane 處理橫幅（選一個選項，或按 Esc 關掉），再重送；排隊的訊息會在橫幅關掉後自動重試。daemon 不會替你按 Esc 或任何鍵。同一次橫幅只推這一則，關掉後又出現才再推。",
    });
    match crate::supervisor::store::push_inbox(&app.db, &key, "ops_alert", None, Some(&run.bot_id), None, &payload).await {
        Ok(Some(_)) => app.emit("supervisor_changed", json!({ "ops_alert": key })).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(run = %run.id, error = %e, "could not queue the codex security banner ops_alert"),
    }
}

/// 擋下派送時同步進 AGM inbox（#789）。同一次橫幅的 event_key 相同，`INSERT OR IGNORE` 不重複；
/// 橫幅消失再出現時 generation 加一，才再推一則。
async fn notify_inbox(app: &Arc<App>, run: &db::Run, gen: u32) {
    let name = sqlx::query_scalar::<_, String>("SELECT name FROM bots WHERE id=?")
        .bind(&run.bot_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| run.bot_id.clone());
    let key = format!("ops_alert:daemon:codex_security_banner:{}:{gen}", run.bot_id);
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": "codex_security_banner",
        "subject": name,
        "bot_id": run.bot_id,
        "bot_name": name,
        "run_id": run.id,
        "detail": NOTICE,
        "action": "請到該 bot 的終端處理橫幅（選一個選項，或按 Esc 關掉），然後重送被擋下的訊息。daemon 沒有代按任何鍵。",
    });
    match crate::supervisor::store::push_inbox(&app.db, &key, "ops_alert", None, Some(&run.bot_id), None, &payload).await {
        Ok(Some(_)) => {}
        Ok(None) => {}
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not queue the codex security banner inbox event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANNER: &str = include_str!("fixtures/codex-0.159.3-security-setup-banner.txt");
    const BANNER_ANSI: &str = include_str!("fixtures/codex-0.159.3-security-setup-banner.ansi");

    #[test]
    fn the_security_banner_blocks_typing_in_plain_and_styled_reads() {
        assert!(blocks_typing(BANNER));
        assert!(blocks_typing(BANNER_ANSI));
        // 動畫粒子落在橫幅周圍的空白列上：抹掉之後照樣認得。
        let sparkled = BANNER_ANSI.replacen("\n\n\u{1b}[1m\u{1b}[36m", "\n\u{1b}[38;2;90;90;200m⠂\u{1b}[0m\n\u{1b}[1m\u{1b}[36m", 1);
        assert_ne!(sparkled, BANNER_ANSI, "要真的放進一顆粒子");
        assert!(blocks_typing(&sparkled));
    }

    #[test]
    fn a_screen_without_a_choice_banner_is_not_blocked() {
        assert!(!blocks_typing("› Reply with PONG\n\n• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n"));
        // 只有資訊、沒有選項：打字就是繼續，數字不會被吃。
        let info = "• PONG\n\n\n  Heads up\n  Something changed on your account.\n\n  esc to dismiss · type to continue\n\n› Ask Codex to do anything\n";
        assert!(!blocks_typing(info));
        // 回覆裡照抄了橫幅的字（結構不完整）不是橫幅。
        let quoted = "› 貼一下提醒的原文\n\n• 原文如下：\n  Keep using Daybreak mode\n  Press a number to choose · esc to dismiss · type to continue\n\n› Ask Codex to do anything\n";
        assert!(!blocks_typing(quoted));
    }

    fn run(id: &str, bot: &str) -> db::Run {
        db::Run {
            id: id.into(),
            bot_id: bot.into(),
            state: "running".into(),
            agent_status: "idle".into(),
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            adopted: 0,
            agent_name: None,
            herdr_session: None,
            agent_title: None,
            status_line: None,
            status_json: None,
            runtime_model: None,
            runtime_effort: None,
            runtime_fast: None,
            runtime_identity: None,
            update_notice: None,
            turn_error: None,
            native_session_id: None,
            transcript_path: None,
            last_read_revision: None,
            last_read_tail_hash: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: None,
            resume_session_id: None,
            resume_outcome: None,
            agent_status_since: None,
            subagent_json: None,
            launch_rev: None,
            live_rev: None,
        }
    }

    /// #789：擋下時 inbox 一則；同一次橫幅再擋不重複；關掉再出現才再推。沒有橫幅不推。
    #[tokio::test]
    async fn blocking_a_delivery_also_notifies_the_agm_inbox_once_per_appearance() {
        let env = crate::testing::env().await;
        let now = crate::db::now();
        let bot = format!("b-{}", crate::db::ulid());
        let project = format!("p-{}", crate::db::ulid());
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES (?,?,?,?)")
            .bind(&project)
            .bind(format!("/tmp/{project}"))
            .bind("p")
            .bind(&now)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?, ?, '安全橫幅', 'codex', 't', ?)")
            .bind(&bot)
            .bind(&project)
            .bind(&bot)
            .bind(&now)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run = run(&format!("r-{}", crate::db::ulid()), &bot);
        let keys = || async {
            sqlx::query_scalar::<_, String>("SELECT event_key FROM supervisor_inbox WHERE bot_id=? ORDER BY created_at")
                .bind(&bot)
                .fetch_all(&env.app.db)
                .await
                .unwrap()
        };

        observe(&env.app, &run, false).await;
        assert!(keys().await.is_empty(), "沒有橫幅不推");

        observe(&env.app, &run, true).await;
        observe(&env.app, &run, true).await;
        let once = keys().await;
        assert_eq!(once.len(), 1, "{once:?}");
        assert!(once[0].starts_with("ops_alert:daemon:codex_security_banner:"), "{}", once[0]);
        let payload: serde_json::Value = serde_json::from_str(
            &sqlx::query_scalar::<_, String>("SELECT payload_json FROM supervisor_inbox WHERE event_key=?")
                .bind(&once[0])
                .fetch_one(&env.app.db)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["bot_id"], bot);
        assert_eq!(payload["bot_name"], "安全橫幅");
        assert_eq!(payload["reason"], "codex_security_banner");
        assert!(payload["action"].as_str().unwrap().contains("重送"), "{payload}");

        observe(&env.app, &run, false).await;
        observe(&env.app, &run, true).await;
        assert_eq!(keys().await.len(), 2, "橫幅再出現要再推一則");
    }
}
