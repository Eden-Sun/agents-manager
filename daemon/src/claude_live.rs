//! claude 在跑的中途被 `/model`、`/effort` 換掉（SPEC §4.4a，2026-09-29 使用者：console-pm 把子 agent requote
//! 從 Opus 換成 Sonnet 5.5，側欄還寫 Opus）。
//!
//! 沒掛 hook 的 run（收編的子 agent）沒有 statusLine 回報，AG Man 只在收編時從 argv 讀過一次模型。claude 執行這兩個
//! slash 指令後會在對話裡印一行確認（2.1.283 真機）：
//!
//! ```text
//!   ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions
//!   ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation …
//! ```
//!
//! 畫面巡邏（`update_watch`，30 秒一輪）順便找最後一次出現的這兩行，寫進 `runs.runtime_*`；子 agent（`managed_by=child`）
//! 的設定本來就是從 argv 抄來的，一併改 `bots.model`／`effort`，側欄才跟著變。
//!
//! 只認 `⎿` 開頭的行（slash 指令的輸出），對話內容裡引用到的同一句不算。一般 bot 可能是 `--resume` 接回來的，
//! 畫面上那行可能是上一個 session 的：**同一個 run 第一次看到的只當基準**，之後變了才採用。子 agent 不會被 AG Man
//! 接回，看到就採用。
//!
//! 例外：AG Man 自己用全新對話起的 run（argv 沒有 `--resume`／`--continue`）畫面上不可能有舊確認，起 run 時就記下空基準
//! （[`start_fresh`]）：第一輪巡邏前使用者打的 `/model` 就是真切換，不能被當基準丟掉（#742）。接回／分支／收編的 run
//! 沒有這個保證，維持「第一次看到只當基準」。

use crate::db;
use crate::state::App;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Switch {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// 畫面上最後一次的 `Set model to …`／`Set effort level to …`（各自取最後一次）。
///
/// `⎿` 不是 slash 指令專屬：工具結果（`⏺ Bash(…)` 底下的 stdout）也用它縮排（#741），所以只認**緊接在使用者打的
/// `❯ /model …`／`❯ /effort …` 之後**的那一行 `⎿`；`⏺` 一出現（工具、回覆）就清掉，一行確認只配一個指令。
/// 指令行必須頂格——工具輸出一律縮排，裡面長得像 `❯ /model` 的行不算。
pub fn parse(screen: &str) -> Switch {
    let mut s = Switch::default();
    let mut after_slash = false;
    for line in screen.lines() {
        if let Some(rest) = line.strip_prefix('❯') {
            let cmd = rest.split_whitespace().next().unwrap_or("");
            after_slash = matches!(cmd, "/model" | "/effort");
            continue;
        }
        if line.starts_with('⏺') {
            after_slash = false;
            continue;
        }
        let Some(rest) = line.trim_start().strip_prefix('⎿') else { continue };
        if !std::mem::take(&mut after_slash) {
            continue;
        }
        let rest = rest.trim_start();
        if let Some(name) = rest.strip_prefix("Set model to ") {
            let name = name.split(" and saved").next().unwrap_or(name);
            let name = name.split(" (").next().unwrap_or(name).trim();
            if let Some(id) = model_id(name) {
                s.model = Some(id);
            }
        } else if let Some(level) = rest.strip_prefix("Set effort level to ") {
            let level: String = level.chars().take_while(char::is_ascii_alphabetic).collect();
            if !level.is_empty() {
                s.effort = Some(level.to_ascii_lowercase());
            }
        }
    }
    s
}

/// 顯示名 → model id：`Sonnet 5.5` → `claude-sonnet-5-5`（`bots.model` 與 argv 用的就是這種）。
fn model_id(display: &str) -> Option<String> {
    let mut it = display.split_whitespace();
    let family = it.next()?.to_ascii_lowercase();
    if !matches!(family.as_str(), "opus" | "sonnet" | "haiku" | "fable") {
        return None;
    }
    let version = it.next().filter(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))?;
    Some(format!("claude-{family}-{}", version.replace('.', "-")))
}

/// run id → 這個 run 第一次看到的切換（基準）。只有一般 bot 會記。
fn baselines() -> &'static Mutex<HashMap<String, Switch>> {
    static B: OnceLock<Mutex<HashMap<String, Switch>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// AG Man 剛用全新對話起了這個 run：畫面上還沒有任何確認行，基準就是「什麼都沒有」，之後第一個看到的切換直接採用。
pub fn start_fresh(run_id: &str) {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string(), Switch::default());
}

#[cfg(test)]
pub(crate) fn has_baseline(run_id: &str) -> bool {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).contains_key(run_id)
}

#[cfg(test)]
pub(crate) fn is_fresh(run_id: &str) -> bool {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).get(run_id) == Some(&Switch::default())
}

/// 這一輪沒看到的 run（結束了）不留帳。
pub fn retain_runs(active: &[String]) {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// statusLine 回報的 `model.id`（#750）：CLI 被 API 拒絕而 server 端 fallback（2.1.286：同級退回上一版）時，argv 與
/// `bots.model` 寫的是 A、實際在跑的是 B，statusLine 的 model 才是權威。有掛 hook 的 claude run 以它校正
/// `runs.runtime_model`（`bots.model` 不動，UI 自然畫成「設定 A、實際 B」，重啟仍照設定值試 A）；下一則又回 A 就收斂回 A。
///
/// 呼叫端已過世代圍籬（舊 run／舊 hook 進不來）。沒有 `model.id`（不從顯示名猜）、空字串、DB 讀寫失敗都保留舊值。
/// 啟動時記的是別名（`opus`、`fable` 這類只有家族名）而 statusLine 回同家族的完整 id：別名本來就由 server 決定指到哪一版，
/// 兩者不算分歧，不改寫（也避免畫面上憑空多出一條「模型」drift；模型專屬額度是以家族判斷的）。
pub async fn adopt_statusline_model(app: &Arc<App>, run: &db::Run, payload: &serde_json::Value) {
    let Some(id) = payload.pointer("/model/id").and_then(|v| v.as_str()).map(str::trim).filter(|m| !m.is_empty()) else {
        return;
    };
    if let Some(cur) = run.runtime_model.as_deref() {
        let alias_of_same_family = matches!(cur, "opus" | "sonnet" | "haiku" | "fable") && id.to_ascii_lowercase().starts_with(&format!("claude-{cur}-"));
        if cur.eq_ignore_ascii_case(id) || alias_of_same_family {
            return;
        }
    }
    // CAS：只在這個 run 還活著、值真的不一樣時寫；同時有別的寫入者（`/model`、live apply）時以資料庫現況為準。
    let wrote = sqlx::query(
        "UPDATE runs SET runtime_model = ? WHERE id = ? AND state IN ('starting','running') AND COALESCE(runtime_model, '') <> ?",
    )
    .bind(id)
    .bind(&run.id)
    .bind(id)
    .execute(&app.db)
    .await;
    match wrote {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!(run = %run.id, from = ?run.runtime_model, to = id, "statusLine reports a different model than the run's recorded one");
            app.emit_bot_status(&run.bot_id).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(run = %run.id, error = %e, "could not record the statusLine model, keeping the old value"),
    }
}

/// 巡邏讀到一份 claude 畫面：有切換而且跟記著的不一樣才寫、才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run, screen: &str) {
    let seen = parse(screen);
    if seen == Switch::default() {
        return;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return };
    let child = bot.managed_by == "child";
    if !child {
        let mut b = baselines().lock().unwrap_or_else(|e| e.into_inner());
        match b.get(&run.id) {
            None => {
                b.insert(run.id.clone(), seen);
                return;
            }
            Some(prev) if *prev == seen => return,
            Some(_) => {
                b.insert(run.id.clone(), seen.clone());
            }
        }
    }
    let Ok(Some(run)) = db::run(&app.db, &run.id).await else { return };
    // run 的落差（跟 runtime 比）與 bot 設定的落差（跟 bots.* 比）分開算（#743）：bot 的 UPDATE 失敗時 runtime 已經對了，
    // 下一輪只剩 bot 落後，仍要補寫，不能因為 runtime 已收斂就當作完成。
    let cur_model = run.runtime_model.clone().or_else(|| bot.model.clone());
    let cur_effort = run.runtime_effort.clone().or_else(|| bot.effort.clone());
    let model = seen.model.clone().filter(|m| cur_model.as_deref() != Some(m.as_str()));
    let effort = seen.effort.clone().filter(|e| cur_effort.as_deref() != Some(e.as_str()));
    let bot_model = seen.model.filter(|m| child && bot.model.as_deref() != Some(m.as_str()));
    let bot_effort = seen.effort.filter(|e| child && bot.effort.as_deref() != Some(e.as_str()));
    let mut changed = false;
    if model.is_some() || effort.is_some() {
        let wrote = sqlx::query("UPDATE runs SET runtime_model = COALESCE(?, runtime_model), runtime_effort = COALESCE(?, runtime_effort) WHERE id = ?")
            .bind(&model)
            .bind(&effort)
            .bind(&run.id)
            .execute(&app.db)
            .await;
        if let Err(e) = wrote {
            tracing::warn!(run = %run.id, bot = %bot.name, error = %e, "could not record the claude runtime switch, retrying next sweep");
            return;
        }
        changed = true;
        tracing::info!(run = %run.id, bot = %bot.name, ?model, ?effort, child, "claude runtime switched in the TUI");
    }
    if bot_model.is_some() || bot_effort.is_some() {
        let wrote = sqlx::query("UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort) WHERE id = ?")
            .bind(&bot_model)
            .bind(&bot_effort)
            .bind(&bot.id)
            .execute(&app.db)
            .await;
        match wrote {
            Ok(_) => changed = true,
            Err(e) => tracing::warn!(run = %run.id, bot = %bot.name, error = %e, "could not follow the claude runtime switch in bots.model/effort, retrying next sweep"),
        }
    }
    if changed {
        app.emit_bot_status(&bot.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = "\
❯ /model sonnet
  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions

❯ /effort high
  ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive
     testing and documentation
";

    #[test]
    fn reads_the_last_model_and_effort_switch() {
        assert_eq!(parse(SCREEN), Switch { model: Some("claude-sonnet-5-5".into()), effort: Some("high".into()) });
        let later = format!("{SCREEN}\n❯ /model opus\n  ⎿  Set model to Opus 5.5 (default)\n");
        assert_eq!(parse(&later).model.as_deref(), Some("claude-opus-5-5"), "取最後一次");
    }

    #[test]
    fn ignores_the_same_words_outside_a_slash_command_output() {
        let quoted = "⏺ 我會執行 /model，畫面會顯示 Set model to Sonnet 5.5\n  Set effort level to max\n";
        assert_eq!(parse(quoted), Switch::default());
        assert_eq!(parse("❯ /model x\n  ⎿  Set model to Something Weird\n"), Switch::default(), "認不出的顯示名不猜");
    }

    #[test]
    fn ignores_tool_output_that_looks_like_model_or_effort_confirmation() {
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";
        assert_eq!(parse(tool_output), Switch::default());
    }

    #[test]
    fn a_real_slash_command_after_tool_output_is_still_recognised() {
        let screen = "⏺ Bash(cat tests.rs)\n  ⎿ Set model to Opus 5.5 and saved as your default\n\n❯ /model sonnet\n  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions\n\n⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to max: spoof\n";
        assert_eq!(parse(screen), Switch { model: Some("claude-sonnet-5-5".into()), effort: None });
        // 工具輸出裡長得像指令行的（縮排）不算，指令後不接 ⎿ 也不留到後面的工具輸出。
        let spoof = "⏺ Bash(cat log)\n    ❯ /model sonnet\n  ⎿ Set model to Sonnet 5.5 and saved\n❯ /model sonnet\n\n⏺ Bash(x)\n  ⎿ Set model to Opus 5.5\n";
        assert_eq!(parse(spoof), Switch::default());
    }

    #[tokio::test]
    async fn a_child_follows_the_switch_and_a_user_bot_only_after_its_baseline() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "子 agent 的設定跟著改");
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-sonnet-5-5"));

        let user = crate::testing::claude_bot(&app, &env.project_id, "user").await;
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model, None, "第一次看到的可能是 --resume 印回來的舊行，只當基準");
        observe(&app, &run, &format!("{SCREEN}\n❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n")).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-haiku-4-5"), "之後變了才採用");
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_ne!(b.model.as_deref(), Some("claude-haiku-4-5"), "一般 bot 的設定不動（重啟會回到設定值，畫成 drift）");
    }

    #[tokio::test]
    async fn a_switch_typed_before_the_first_sweep_of_a_fresh_run_is_adopted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let user = crate::testing::claude_bot(&app, &env.project_id, "fresh").await;
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        start_fresh(&run_id);
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "model 與 effort 都在第一輪就採用");
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_eq!(b.model, None, "一般 bot 的設定不動");
    }

    /// #743：run 的 UPDATE 成功、bot 的 UPDATE 失敗，下一輪要把 bot 補上（runtime 已經對了也一樣）。
    #[tokio::test]
    async fn a_child_whose_bot_update_failed_is_caught_up_on_the_next_sweep() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid743").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        // 讓 bot 的 UPDATE 失敗（run 的不受影響）。
        sqlx::query("CREATE TRIGGER fail_bot_update BEFORE UPDATE ON bots BEGIN SELECT RAISE(ABORT, 'boom'); END")
            .execute(&app.db)
            .await
            .unwrap();
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "run 已寫入");
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-opus-5-5"), Some("xhigh")), "bot 的 UPDATE 失敗，還沒跟上");

        sqlx::query("DROP TRIGGER fail_bot_update").execute(&app.db).await.unwrap();
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "下一輪補上 model 與 effort");
    }

    /// 2.1.286 同級 fallback：argv／設定是 Opus 5.5，server 退回上一版（statusLine 回的 id 不同）。
    fn statusline(model: serde_json::Value) -> crate::hookrecv::HookBody {
        crate::hookrecv::HookBody {
            bot_id: String::new(),
            provider: "claude".into(),
            payload: serde_json::json!({"hook_event_name": "StatusLine", "session_id": "s-750", "model": model}),
            received_at: None,
            truncated: false,
            run_id: None,
        }
    }

    #[tokio::test]
    async fn the_statusline_model_corrects_runtime_model_but_never_the_configured_one() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "fallback750").await;
        sqlx::query("UPDATE bots SET model = 'claude-opus-5-5' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model = 'claude-opus-5-5' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        let runtime = || async { db::run(&app.db, &run_id).await.unwrap().unwrap().runtime_model };
        let send = |model: serde_json::Value| {
            let mut body = statusline(model);
            body.bot_id = bot.id.clone();
            let app = app.clone();
            async move { crate::hookrecv::process(&app, &body).await.unwrap() }
        };

        send(serde_json::json!({"id": "claude-opus-5", "display_name": "Opus 5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5"), "實際在跑的是 fallback 後那一版");
        let b = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(b.model.as_deref(), Some("claude-opus-5-5"), "設定值不動，UI 才畫得出 drift");

        // 沒有 id（只有顯示名）、空 id、沒有 model：保留舊值，不清空、不猜。
        send(serde_json::json!({"display_name": "Opus 5.5"})).await;
        send(serde_json::json!({"id": "  "})).await;
        send(serde_json::Value::Null).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5"));

        send(serde_json::json!({"id": "claude-opus-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5-5"), "下一則又回設定的那一版就收斂回去");

        // 啟動時記的是別名：同家族的完整 id 不算分歧。
        sqlx::query("UPDATE runs SET runtime_model = 'opus' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        send(serde_json::json!({"id": "claude-opus-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("opus"));
        send(serde_json::json!({"id": "claude-sonnet-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-sonnet-5-5"), "別家族一定是真的換了");
    }

    #[tokio::test]
    async fn a_stale_statusline_cannot_overwrite_the_current_run_and_a_db_error_keeps_the_old_value() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "stale750").await;
        let old = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ?, runtime_model = 'claude-opus-5-5', native_session_id = 's-old' WHERE id = ?")
            .bind(db::now())
            .bind(&old)
            .execute(&app.db)
            .await
            .unwrap();
        let cur = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model = 'claude-opus-5-5', native_session_id = 's-cur' WHERE id = ?").bind(&cur).execute(&app.db).await.unwrap();

        // 舊 session 的 statusLine（世代圍籬擋下）不能改到現在這個 run。
        let mut body = statusline(serde_json::json!({"id": "claude-haiku-4-5"}));
        body.bot_id = bot.id.clone();
        body.payload["session_id"] = serde_json::json!("s-old");
        crate::hookrecv::process(&app, &body).await.unwrap();
        let r = db::run(&app.db, &cur).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-opus-5-5"));

        // DB 寫入失敗：保留舊值（不清空）。
        sqlx::query("CREATE TRIGGER refuse_runtime BEFORE UPDATE OF runtime_model ON runs BEGIN SELECT RAISE(ABORT, 'boom'); END")
            .execute(&app.db)
            .await
            .unwrap();
        adopt_statusline_model(&app, &r, &serde_json::json!({"model": {"id": "claude-opus-5"}})).await;
        let r = db::run(&app.db, &cur).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-opus-5-5"));
    }

    #[tokio::test]
    async fn tool_output_does_not_change_a_child_runtime_or_configured_model() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";

        observe(&app, &run, tool_output).await;

        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-opus-5-5"), Some("xhigh")));
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (None, None));
    }
}
