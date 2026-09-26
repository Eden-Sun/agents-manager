//! 角色 bot（巡檢／協調者）「現在能不能用」的畫面與 notify 證據（issue #427，#420 的後續）。
//!
//! 為什麼另外一個模組：[`crate::supervisor::health::role_state`] 只讀 DB——`/api/supervisor/health`
//! 與 `/api/supervisor/responder` 都會被 UI 高頻輪詢，每次都去抓一次 pane 會把 herdr 打爆。
//! 這裡是**每 30 秒一次**的那一路：health tick 在 `incidents::sweep` 之前呼叫 [`refresh`]，
//! 把結論放記憶體，`role_state` 只讀記憶體。
//!
//! 兩個缺口就是這張票要補的（#427 第 2 項）：**巡檢從來沒有被看過**（它是 incident 與 ops_alert
//! 的收件人，停在登入失效一樣沒有人知道），而協調者只有在「有事要送、而且送不出去」時才會被看
//! （`responder::notify` 的錯誤路徑，issue #420）——佇列空著時它壞了也要等到下一次有事才發現。
//!
//! 結論**不進 DB**：跟 incident 的門檻計時同一個原則（SPEC §18.9「計時在記憶體，重啟重算」）。
//! 寫進 DB 的故障會跟著重啟活過來，而重啟後第一拍就能重新判定——寧可晚 30 秒開，也不要讓一顆
//! 已經修好的協調者因為陳舊的旗標被繼續當成壞的。同理，**空表不是故障**：daemon 剛起來、第一拍
//! 還沒跑時 [`reason`] 回 `None`，`role_state` 就退回原本只看 DB 的行為（#421 的不變量）。

use crate::state::App;
use crate::supervisor::roles::Role;
use std::sync::Arc;

/// 停在登入失效。契約字串的正本在 [`crate::supervisor::health`]（#421 原樣寫進 inbox payload），
/// 這裡不另外定義一份——同一個字串兩個來源，哪天漂掉沒有人會發現。
pub use crate::supervisor::health::REASON_NEEDS_LOGIN;

/// 連續 [`NOTIFY_STALL_LIMIT`] 個 notify 回合沒完成（#427 第 3 項）。「送出去了、但回合沒跑完」，
/// 跟 `responder_undeliverable`（根本送不出去）是兩件事。
pub const REASON_NOTIFY_STALLED: &str = "notify_stalled";

/// 連續幾個**不同的** notify 回合沒完成就算不可用。#420 的現場是被送了 66／43／18／6 次才 gave_up；
/// 三次（約一分半）就該有人知道。同一個回合被 `recover_unacked` 掃到好幾次只算一次。
pub const NOTIFY_STALL_LIMIT: usize = 3;

/// 兩個角色都要看。巡檢沒有被看是 #427 第 2 項明講的缺口之一。
pub const WATCHED: [Role; 2] = [Role::Responder, Role::Patrol];

/// 一顆角色 bot 這一拍看到的故障。存在 [`crate::state::App::role_faults`]（記憶體，重啟重算）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleFault {
    /// `None` = 這一拍沒看到問題。值一定是 [`REASON_NEEDS_LOGIN`] 或 [`REASON_NOTIFY_STALLED`]。
    pub reason: Option<&'static str>,
    /// 已經算進來、而且沒跑完的 notify 回合 id。
    ///
    /// **是集合不是計數器**：`recover_unacked` 是對 inbox 事件**逐筆**呼叫的，同一個角色一輪裡可以有
    /// 好幾筆事件、各自不同的 `notify_turn_id`。用「上一個算過的 id」去重的話，兩筆交錯的壞回合 A／B
    /// 會這樣走：第一輪 A→1、B→2，第二輪 A 又跟上一個（B）不同→3——只有兩個壞回合、兩輪就到門檻，
    /// 跟「連續三個**不同的**回合」不符，而這個數字直接餵 `role_state` → `is_unavailable()` →
    /// #421 的改派閘門（i407 review 2026-09-24）。
    pub failed_turns: std::collections::BTreeSet<String>,
    /// 這一拍連畫面都讀不到（沒有 run、herdr 沒回、pane 不見了）。**不是故障**——
    /// 沒有證據不能當成「它壞了」（#421 的不變量），incident 那邊用它決定不開也不關。
    pub probe_failed: bool,
    /// 第一次看到這個 `reason` 的時間，給 incident 的 detail 用。
    pub since: Option<String>,
}

/// 每一拍看一次兩顆角色 bot 的畫面，把結論寫進記憶體。
///
/// 由 health 的 30 秒 tick 在 `incidents::sweep` **之前**呼叫：incident 與同一拍的 health 讀數要講
/// 同一件事（沿用那個 tick 原本的註解「a fault and the health reading that mentions it never
/// disagree by one tick」）。
pub async fn refresh(app: &Arc<App>) {
    for role in WATCHED {
        // 沒建立的角色整個不留紀錄：留著會讓 `incidents::observe` 每一拍都把這個 kind 算成
        // 「探針沒跑」，另一顆角色已經開著的 incident 就再也關不掉（blind 會擋住 resolve）。
        if !is_configured(app, role).await {
            app.role_faults.lock().await.remove(role.as_str());
            continue;
        }
        let probe = probe_role(app, role).await;
        let mut faults = app.role_faults.lock().await;
        let entry = faults.entry(role.as_str().to_string()).or_default();
        entry.probe_failed = probe.is_none();
        // 兩個訊號各自算，不要混在同一個 match 裡：**只有畫面那一半**在讀不到時要維持上一拍的結論
        // （不憑空宣告故障、也不宣告恢復）；notify 那一半的證據就在手上的集合裡，每一拍都要重算。
        // 混在一起的話，notify 恢復（集合被清空）那一拍剛好讀不到畫面，就會把已經解除的
        // `notify_stalled` 當成「上一拍的結論」繼續留著，然後每一拍都讀不到、每一拍都繼續留著。
        let screen_says_logged_out = match probe {
            Some(v) => v,
            None => entry.reason == Some(REASON_NEEDS_LOGIN),
        };
        let reason = if screen_says_logged_out {
            Some(REASON_NEEDS_LOGIN)
        } else if entry.failed_turns.len() >= NOTIFY_STALL_LIMIT {
            // 畫面是好的（或看不到），但 notify 一直沒完成也算不可用（#427 第 3 項）。
            Some(REASON_NOTIFY_STALLED)
        } else {
            None
        };
        if entry.reason != reason {
            entry.since = reason.map(|_| crate::db::now());
            match reason {
                Some(r) => tracing::warn!(role = role.as_str(), reason = r, "角色 bot 不可用"),
                None => tracing::info!(role = role.as_str(), "角色 bot 恢復"),
            }
        }
        entry.reason = reason;
    }
}

/// 這一拍的結論：不可用的原因，沒看到問題（或表還沒被填過）就是 `None`。
pub async fn reason(app: &Arc<App>, role: Role) -> Option<&'static str> {
    app.role_faults.lock().await.get(role.as_str()).and_then(|f| f.reason)
}

/// incident 那邊要的整份（`probe_failed`／`since`／`failed_turns` 都要，不只 `reason`）。
pub async fn snapshot(app: &Arc<App>, role: Role) -> Option<RoleFault> {
    app.role_faults.lock().await.get(role.as_str()).cloned()
}

/// 記一**輪** `recover_unacked` 對某個角色看到的 notify 結果：`failed` 是這一輪掃到、回合是 `failed`
/// 的那些 turn id，`any_completed` 是這一輪有沒有任何一個回合真的跑完。
///
/// **以一輪為單位、不是逐筆**：逐筆呼叫時「一筆壞、一筆好」的結果會取決於迭代順序（先好後壞是 1，
/// 先壞後好是 0）。以輪為單位就只有一個答案：這一輪只要有回合跑完，notify 這條路就是通的，整個歸零；
/// 否則把壞的 id 併進集合，連續 [`NOTIFY_STALL_LIMIT`] 個**不同的**回合才算不可用。
///
/// 只累計，不在這裡下結論：`reason` 一律由 [`refresh`] 在同一拍統一算，免得兩個地方各寫各的。
pub async fn note_notify_round(
    app: &Arc<App>,
    role: &str,
    failed: std::collections::BTreeSet<String>,
    any_completed: bool,
) {
    // 只認這兩個角色：別的字串進來就不要在表裡長出永遠不會被 `refresh` 清掉的項目。
    if !WATCHED.iter().any(|r| r.as_str() == role) {
        return;
    }
    if failed.is_empty() && !any_completed {
        return;
    }
    let mut faults = app.role_faults.lock().await;
    let entry = faults.entry(role.to_string()).or_default();
    if any_completed {
        entry.failed_turns.clear();
        return;
    }
    let before = entry.failed_turns.len();
    entry.failed_turns.extend(failed);
    if entry.failed_turns.len() != before {
        tracing::warn!(role, failures = entry.failed_turns.len(), "notify 回合沒完成");
    }
}

/// 這個角色建立過沒有（有登記的 bot_id）。讀不到就當成「有」——當成沒有會把已經開著的 incident
/// 誤關掉，而 `role_state` 那邊讀不到 DB 本來就會回 `Unknown`。
async fn is_configured(app: &Arc<App>, role: Role) -> bool {
    match crate::supervisor::roles::get(&app.db, role).await {
        Ok(row) => row.bot_id.is_some(),
        Err(_) => true,
    }
}

/// 這顆角色 bot 是不是停在登入失效；讀不到回 `None`（＝這一拍沒有證據）。
///
/// **兩個訊號都要成立**：
/// 1. 畫面的回覆槽那一行（`tui_prompts::is_not_logged_in_reply`），以及
/// 2. daemon 自己那份額度讀數是空的或陳舊的（[`quota_is_blank`]）。
///
/// 為什麼要第二個：第一個訊號本來只在「送不出去時才看畫面」那條路上用（`responder::notify` 的錯誤
/// 路徑，issue #420），那個前提本身就是很強的守衛。#427 第 2 項改成每一拍都看、而且巡檢也看，前提
/// 就沒了——一顆完全正常的 bot 只要在回報裡引用那句話就會被判成故障。
///
/// 第二個訊號刻意**不看畫面**：statusLine 上那兩格額度是使用者自己的 `statusline-command.sh` 印的，
/// 認它的字面（`5h:-`）等於把偵測綁在一支外部腳本的格式上，換一台沒有那支腳本的機器就永遠不成立，
/// 於是真的登出也偵測不到，而且測試照不出來（i407 review 2026-09-24）。daemon 自己那份是 claude 的
/// StatusLine hook 送進來的 `rate_limits`（`/api/quota` 同一個來源），登入不了就不會有新的讀數。
async fn probe_role(app: &Arc<App>, role: Role) -> Option<bool> {
    let row = crate::supervisor::roles::get(&app.db, role).await.ok()?;
    let bot_id = row.bot_id?;
    // claude 以外的 CLI 沒有這個畫面，不要拿別人的版面去猜——但那不是「讀不到」，是「不是這個問題」。
    let bot = crate::db::bot(&app.db, &bot_id).await.ok().flatten()?;
    if bot.kind != "claude" {
        return Some(false);
    }
    let run = crate::db::active_run(&app.db, &bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = app.herdr_for_run(&run).await?;
    let read = client.pane_read(&pane, "visible", 80).await.ok()?;
    if !crate::tui_prompts::is_not_logged_in_reply(&read.text) {
        return Some(false);
    }
    quota_is_blank(app, &bot).await
}

/// daemon 自己那份額度讀數是不是「空的」：整把 key 不在表裡、或它只是開機從 `quota_cache` 回填的
/// 陳舊值（`App.quota_stale`，下一次真的探測成功才會被清掉）、或兩個視窗都沒有數字。
///
/// 登入失效的 CLI 跑不完任何一個回合，也就送不出 StatusLine hook，所以它的讀數只會停在舊的那一份；
/// 而在回報裡引用那句話的 bot 是**剛跑完一個回合**才印得出那份回報，那一回合就會帶一份新的讀數進來。
/// 這正是兩者的差別，而且完全不依賴畫面上印了什麼。
///
/// 讀不到這顆 bot 在哪台主機就回 `None`（#243）：當成本機會拿 daemon 這台的讀數去判遠端 bot 登入失效。
async fn quota_is_blank(app: &Arc<App>, bot: &crate::db::Bot) -> Option<bool> {
    let host = crate::db::bot_host(&app.db, &bot.id).await.ok()?;
    let base = crate::quota::quota_base_for_host(app, &host, &bot.kind, bot.identity.as_deref()).await;
    let key = crate::quota::quota_key(&host, &base);
    if app.quota_stale.lock().await.contains(&key) {
        return Some(true);
    }
    Some(match app.quotas.lock().await.get(&key) {
        None => true,
        Some(q) => q.five_hour.is_none() && q.seven_day.is_none(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONDER: Role = Role::Responder;

    async fn configure(app: &Arc<App>, role: Role) {
        sqlx::query("INSERT OR IGNORE INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = format!("bot-{}", role.as_str());
        sqlx::query("INSERT OR IGNORE INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude',?,?)")
            .bind(&bot)
            .bind(&bot)
            .bind(format!("tok-{bot}"))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::supervisor::roles::set_env(&app.db, role, &bot, "p", "/tmp").await.unwrap();
    }

    fn turns(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// #427 第 3 項：門檻是「連續 [`NOTIFY_STALL_LIMIT`] 個**不同的**回合」。
    ///
    /// 這一條釘的是 i407 抓到的那個算錯（2026-09-24）：`recover_unacked` 對 inbox 事件**逐筆**呼叫，
    /// 同一個角色一輪裡可以有好幾筆事件、各自不同的 `notify_turn_id`。用「上一個算過的 id」去重時，
    /// 兩筆交錯的壞回合 A／B 兩輪就會湊到 3（A→1、B→2，第二輪 A 又跟上一個不同→3），
    /// 而這個數字直接餵 `role_state` → `is_unavailable()` → #421 的改派閘門。
    #[tokio::test]
    async fn two_interleaved_broken_turns_never_reach_the_threshold() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        for _ in 0..5 {
            note_notify_round(&app, "responder", turns(&["A", "B"]), false).await;
        }
        assert_eq!(
            snapshot(&app, RESPONDER).await.unwrap().failed_turns.len(),
            2,
            "只有兩個壞回合，掃幾輪都還是兩個"
        );

        // 第三個**不同的**回合才到門檻。
        note_notify_round(&app, "responder", turns(&["C"]), false).await;
        assert_eq!(snapshot(&app, RESPONDER).await.unwrap().failed_turns.len(), NOTIFY_STALL_LIMIT);
    }

    /// 同一輪裡一筆壞、一筆跑完時答案只有一個，不跟著迭代順序跑：有回合跑完就是這條路通了，整個歸零。
    #[tokio::test]
    async fn a_round_with_any_completed_turn_resets_regardless_of_order() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        note_notify_round(&app, "responder", turns(&["A", "B"]), false).await;
        note_notify_round(&app, "responder", turns(&["C"]), true).await;
        assert!(snapshot(&app, RESPONDER).await.unwrap().failed_turns.is_empty(), "這一輪有回合跑完＝通的");

        // 不是角色的事件（一般 bot）不要在表裡長出永遠不會被 refresh 清掉的項目。
        note_notify_round(&app, "some-bot", turns(&["t9"]), false).await;
        assert!(app.role_faults.lock().await.get("some-bot").is_none());
        // 什麼都沒看到的一輪不要無中生有一個項目。
        note_notify_round(&app, "patrol", turns(&[]), false).await;
        assert!(snapshot(&app, Role::Patrol).await.is_none());
    }

    /// #421 的不變量：**讀不到畫面不能變成「它壞了」**。沒有 active run 時探針回不了答案，
    /// 這一拍只記 `probe_failed`，`reason` 仍然是 `None`——`role_state` 因此維持原本只看 DB 的行為。
    #[tokio::test]
    async fn a_probe_that_cannot_read_the_screen_is_not_a_fault() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;

        refresh(&app).await;
        let f = snapshot(&app, RESPONDER).await.expect("建立過的角色要有項目");
        assert!(f.probe_failed, "沒有 active run＝這一拍沒有證據");
        assert_eq!(f.reason, None, "沒有證據不能宣告故障");
        assert_eq!(reason(&app, RESPONDER).await, None);
    }

    /// 畫面讀不到、但 notify 已經連續 [`NOTIFY_STALL_LIMIT`] 次沒完成：這是**另一個**訊號，
    /// 不依賴畫面，所以照樣要判不可用（#420 現場就是「看起來 idle、但每個回合都失敗」）。
    #[tokio::test]
    async fn notify_stalling_is_a_fault_even_when_the_screen_cannot_be_read() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;

        note_notify_round(&app, "responder", turns(&["t1", "t2"]), false).await;
        refresh(&app).await;
        assert_eq!(reason(&app, RESPONDER).await, None, "還沒到門檻");

        note_notify_round(&app, "responder", turns(&["t3"]), false).await;
        refresh(&app).await;
        assert_eq!(reason(&app, RESPONDER).await, Some(REASON_NOTIFY_STALLED));
        assert!(snapshot(&app, RESPONDER).await.unwrap().since.is_some(), "要記下第一次看到的時間");

        // 回合跑完就歸零，下一拍自己恢復——不用任何人來按。**畫面照樣讀不到**：notify 那一半的證據
        // 在集合裡，不該因為看不到畫面就把已經解除的結論繼續留著。
        note_notify_round(&app, "responder", turns(&[]), true).await;
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.unwrap().probe_failed, "畫面仍然讀不到");
        assert_eq!(reason(&app, RESPONDER).await, None);
    }

    /// 沒建立的角色**整個不留項目**：留著會讓 `incidents::observe` 每一拍把這個 kind 算成
    /// 「探針沒跑」，另一顆角色已經開著的 incident 就再也關不掉（blind 會擋住 resolve）。
    #[tokio::test]
    async fn a_role_that_was_never_set_up_leaves_no_entry_behind() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_some());
        assert!(snapshot(&app, Role::Patrol).await.is_none(), "巡檢還沒建立：不留紀錄");

        // 角色被拆掉（bot 刪了、登記清空）之後，上一輪留下的項目也要跟著消失。
        sqlx::query("UPDATE supervisor_roles SET bot_id=NULL").execute(&app.db).await.unwrap();
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_none(), "不再建立就不留陳舊的結論");
    }

    /// #243：讀不到 bot 在哪台主機時不能拿本機的額度讀數去判——本機那把 key 對遠端 bot 永遠是空的，
    /// 會把一顆正常的遠端角色判成登入失效。回 `None`＝這一拍沒有證據。
    #[tokio::test]
    async fn an_unreadable_host_is_no_evidence_of_a_blank_quota() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        sqlx::query("UPDATE projects SET host='m4p' WHERE id='p'").execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, "bot-responder").await.unwrap().unwrap();

        crate::testing::make_table_unreadable(&app, "projects").await;
        let seen = quota_is_blank(&app, &bot).await;
        crate::testing::make_table_readable(&app, "projects").await;
        assert_eq!(seen, None, "讀不到主機不是「額度空的」");
        assert_eq!(quota_is_blank(&app, &bot).await, Some(true), "讀得到之後照常判（遠端這把 key 沒有讀數）");
    }
}
