//! 子 agent 卡在提問時，告訴它的父 agent（使用者 2026-09-18）。
//!
//! 側欄上那些 `!2` / `!3` 與「等小孩」圓點是 [`crate::api`] 投影給**人**看的；父 agent 是一顆 CLI
//! 行程，除非有人把字打進它的 pane，否則它永遠不知道自己的 child 停在那裡等回答——它自己的回合
//! 早就結束了。使用者只好手動打一句「你 child 又問了，回答他阿」。
//!
//! 所以：child 轉成 `blocked` 並且**穩定**幾秒之後，daemon 用 `relay_from = <child bot id>` 送一則
//! 進父 agent 的對話。走既有的 [`crate::lifecycle::prompt_relayed_queueable`]：父 agent 正在回合中
//! 就排隊，不插隊、不打斷。
//!
//! 幾條「不吵人」的界線：
//! * daemon 自己會按掉的畫面不算（滿意度問卷、`/model` 確認框）——那些幾秒內就消失了；
//! * 同一個問題只講一次（畫面尾段的指紋），child 在同一個提問上重畫不會變成連珠炮；
//! * 父 agent 沒有活著的 run 就不送：沒有 pane 可以收，UI 的徽章仍在，使用者看得到；
//! * 只有 `managed_by = 'child'` 且真的有 `parent_bot_id` 的 bot 會觸發。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::db;
use crate::state::App;

/// 轉成 `blocked` 之後等這麼久才看：`dismiss_if_survey` 與 `/model` 確認框都在這段時間內處理完，
/// 使用者自己在 pane 裡回答掉也來得及。
const SETTLE: Duration = Duration::from_secs(8);

/// 訊息裡最多帶這麼多字的畫面尾段——父 agent 要的是「它在問什麼」，不是整個終端。
const MAX_QUESTION_CHARS: usize = 600;

/// 已經替哪一顆 child 講過哪一個問題（`bot_id -> 指紋`）。存在記憶體：daemon 重啟後最多重講一次，
/// 比為了這個加一張表划算。
fn spoken() -> &'static Mutex<HashMap<String, u64>> {
    static V: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 畫面尾段壓成「它在問什麼」。
///
/// 只取最後幾行有內容的：真正在等人回答的東西就畫在輸入列上面，再往上是正文。框線、游標、
/// statusLine 與 `⏵⏵ bypass permissions` 這類固定行丟掉——那些每回合都在變，會讓指紋一直不同。
pub fn question_from_screen(screen: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in screen.lines() {
        let line: String = raw
            .chars()
            .map(|c| if "│┌┐└┘─├┤┬┴┼╭╮╯╰▎▔".contains(c) { ' ' } else { c })
            .collect();
        let line = line.trim().to_string();
        if line.is_empty() || is_chrome(&line) {
            continue;
        }
        lines.push(line);
    }
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let tail = tail.trim();
    if tail.is_empty() {
        return None;
    }
    Some(truncate(tail, MAX_QUESTION_CHARS))
}

/// 每回合都在變、對父 agent 沒有意義的固定行。
fn is_chrome(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.starts_with("⏵⏵")
        || l.contains("bypass permissions on")
        || l.contains("shift+tab to cycle")
        // 使用者的 statusLine（`名字 | 專案 | 模型 31% | 5h:96%`）：每回合都在變，帶進來會讓
        // 同一個問題每次算出不同指紋，變成連珠炮。
        || (l.contains('|') && l.contains('%'))
        || l.starts_with('❯') && l.len() <= 2
        || l.chars().all(|c| c == '>' || c == '❯' || c.is_whitespace())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{cut}…")
}

fn fingerprint(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 這個畫面該不該吵父 agent。`None` ＝不該。
///
/// daemon 自己會按掉的畫面不算：問卷（`0: Dismiss`）與 `/model`／`/effort` 的確認框（`tui_prompts`
/// 會替使用者按掉，幾秒後就不見了）。
pub fn alertable_question(screen: &str) -> Option<String> {
    if crate::tui_prompts::is_feedback_survey(screen) || crate::tui_prompts::is_switch_model_dialog(screen) {
        return None;
    }
    question_from_screen(screen)
}

/// 送給父 agent 的那一則。
pub fn message_for(child_name: &str, question: &str) -> String {
    format!(
        "[子 agent {child_name} 停著在等回答]\n{question}\n\n（daemon 自動通知：它的回合停在 blocked。要回它就用 `herdr prompt {child_name} \"…\"`，或在它的分頁直接回。）"
    )
}

/// child 轉成 `blocked` 時呼叫（[`crate::events::handle_status`]）。自己開背景工作，不擋事件迴圈。
pub fn on_child_blocked(app: &Arc<App>, run: &db::Run) {
    if cfg!(test) {
        return;
    }
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        if let Err(e) = tell_parent(&app, &run).await {
            tracing::debug!(bot = %run.bot_id, error = %e, "could not tell the parent about its blocked child");
        }
    });
}

/// 這顆 child 現在該不該通知、通知誰。回 `(父 bot id, child)`；不該通知就是 `None`。
///
/// 每一條界線都從 DB 讀，所以測得到：狀態已經不是 blocked（被回答／被按掉／停掉）、不是子 agent、
/// 已刪、沒有父、父沒有活著的 run（沒有 pane 收這則，UI 徽章仍在）。
pub async fn parent_to_tell(app: &Arc<App>, bot_id: &str) -> anyhow::Result<Option<(String, db::Bot)>> {
    let Some(fresh) = db::active_run(&app.db, bot_id).await? else { return Ok(None) };
    if fresh.agent_status != "blocked" {
        return Ok(None);
    }
    let Some(child) = db::bot(&app.db, bot_id).await? else { return Ok(None) };
    if child.managed_by != "child" || child.deleted_at.is_some() {
        return Ok(None);
    }
    let Some(parent_id) = child.parent_bot_id.clone().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if db::active_run(&app.db, &parent_id).await?.is_none() {
        return Ok(None);
    }
    Ok(Some((parent_id, child)))
}

async fn tell_parent(app: &Arc<App>, run: &db::Run) -> anyhow::Result<()> {
    let Some((parent_id, child)) = parent_to_tell(app, &run.bot_id).await? else { return Ok(()) };
    let Some(fresh) = db::active_run(&app.db, &run.bot_id).await? else { return Ok(()) };
    let Some(pane) = fresh.pane_id.clone().filter(|p| !p.trim().is_empty()) else { return Ok(()) };
    let Some(client) = app.herdr_for_run(&fresh).await else { return Ok(()) };
    let screen = client.pane_read(&pane, "visible", 60).await?.text;
    let Some(question) = alertable_question(&screen) else { return Ok(()) };

    let fp = fingerprint(&question);
    {
        let mut seen = spoken().lock().unwrap();
        if seen.get(&child.id) == Some(&fp) {
            return Ok(());
        }
        seen.insert(child.id.clone(), fp);
    }

    let crid = format!("child-blocked:{}:{fp:x}", child.id);
    match crate::lifecycle::prompt_relayed_queueable(app, &parent_id, &message_for(&child.name, &question), &crid, Some(&child.id)).await {
        Ok(out) => tracing::info!(child = %child.name, parent = %parent_id, delivery = %out.delivery, "told the parent its child is waiting"),
        Err(e) => {
            // 送不出去就把指紋收回來，下一次事件再試一次。
            spoken().lock().unwrap().remove(&child.id);
            tracing::debug!(child = %child.name, parent = %parent_id, error = ?e, "the parent could not be told");
        }
    }
    Ok(())
}

/// child 不再 blocked 時把指紋忘掉：同一個問題**再次**出現（例如它又問一次）才會再講一次。
pub fn forget(bot_id: &str) {
    spoken().lock().unwrap().remove(bot_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真的在等人回答的畫面：帶得出問題本身，而不是整個終端。
    const PERMISSION: &str = "\
⏺ 我先把設定檔改好再跑測試。
⏺ Bash(rm -rf ./target/debug)
╭──────────────────────────────────────╮
│  Do you want to proceed?             │
│  ❯ 1. Yes                            │
│    2. No, and tell Claude what to do  │
╰──────────────────────────────────────╯
  tony. | agents-manager | Opus 5 31% | 5h:96%
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    #[test]
    fn the_question_is_what_the_parent_gets_not_the_whole_terminal() {
        let q = alertable_question(PERMISSION).expect("這種畫面要通知");
        assert!(q.contains("Do you want to proceed?"), "{q}");
        assert!(q.contains("1. Yes"), "{q}");
        // statusLine 與 bypass 那行每回合都在變，帶進來會讓指紋一直不同。
        assert!(!q.contains("bypass permissions"), "{q}");
        assert!(!q.contains("5h:96%"), "{q}");
        // 框線不要。
        assert!(!q.contains('│'), "{q}");
    }

    /// daemon 自己會按掉的畫面不吵人：問卷與換模型確認框。
    #[test]
    fn dialogs_the_daemon_answers_itself_are_not_worth_a_message() {
        let survey = " ● How is Claude doing this session? (optional)\n   1: Bad    2: Fine   3: Good   0: Dismiss\n";
        assert!(alertable_question(survey).is_none());
        let switch = "   Switch model?\n   Your next response will be slower and use more tokens\n   ❯ 1. Yes, switch to Haiku 4.5\n     2. No, go back\n";
        assert!(alertable_question(switch).is_none());
        assert!(alertable_question("").is_none());
        assert!(alertable_question("   \n  \n").is_none());
    }

    /// 同一個問題只講一次；問題變了才再講。指紋認的是內容，不是時間。
    #[test]
    fn the_same_question_is_only_said_once() {
        let a = alertable_question(PERMISSION).unwrap();
        let again = alertable_question(&format!("{PERMISSION}  ⏵⏵ bypass permissions on\n")).unwrap();
        assert_eq!(fingerprint(&a), fingerprint(&again), "只有每回合都在變的那幾行不同，不該算成新問題");

        let other = alertable_question("╭────╮\n│ 要不要我順便把它推上去？ │\n╰────╯\n").unwrap();
        assert_ne!(fingerprint(&a), fingerprint(&other));
    }

    /// 界線真的打到 DB：只有「blocked 的子 agent，而且父還活著」才通知。
    #[tokio::test]
    async fn only_a_blocked_child_with_a_live_parent_is_worth_telling() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let now = db::now();
        let mk = |id: &'static str, managed: &'static str, parent: Option<&'static str>| {
            let app = app.clone();
            let project = e.project_id.clone();
            let now = now.clone();
            async move {
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     VALUES (?,?,?,'claude','[]',0,1,?,?,?,?)",
                )
                .bind(id).bind(&project).bind(id).bind(format!("tok-{id}")).bind(managed).bind(parent).bind(&now)
                .execute(&app.db).await.unwrap();
            }
        };
        mk("parent1", "user", None).await;
        mk("kid", "child", Some("parent1")).await;
        mk("orphan", "child", None).await;
        mk("own", "user", None).await;
        for bot in ["kid", "orphan", "own"] {
            crate::testing::fake_run(&app, bot).await;
            sqlx::query("UPDATE runs SET agent_status='blocked' WHERE bot_id=?").bind(bot).execute(&app.db).await.unwrap();
        }

        // 父還沒起來：沒有 pane 收，先不吵（UI 的徽章仍在）。
        assert!(parent_to_tell(&app, "kid").await.unwrap().is_none(), "父沒在跑就不送");

        crate::testing::fake_run(&app, "parent1").await;
        let (parent, child) = parent_to_tell(&app, "kid").await.unwrap().expect("父活著就要通知");
        assert_eq!(parent, "parent1");
        assert_eq!(child.name, "kid");

        // 不是子 agent、沒有父、已經不是 blocked 的都不送。
        assert!(parent_to_tell(&app, "own").await.unwrap().is_none());
        assert!(parent_to_tell(&app, "orphan").await.unwrap().is_none());
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id='kid'").execute(&app.db).await.unwrap();
        assert!(parent_to_tell(&app, "kid").await.unwrap().is_none(), "已經被回答就不送");
    }

    /// 訊息本身要講得出「是誰、怎麼回」——父 agent 收到的就是這一段字。
    #[test]
    fn the_message_says_who_is_waiting_and_how_to_answer() {
        let m = message_for("am-m3-fix", "Do you want to proceed?");
        assert!(m.contains("am-m3-fix"), "{m}");
        assert!(m.contains("Do you want to proceed?"), "{m}");
        assert!(m.contains("herdr prompt am-m3-fix"), "{m}");
    }

    /// 太長的畫面要截斷，不要把整個終端塞進父 agent 的對話。
    #[test]
    fn a_long_screen_is_truncated() {
        let long = format!("╭──╮\n{}\n╰──╯\n", "這是一段很長的輸出。".repeat(200));
        let q = alertable_question(&long).unwrap();
        assert!(q.chars().count() <= MAX_QUESTION_CHARS + 1, "{}", q.chars().count());
        assert!(q.ends_with('…'));
    }
}
