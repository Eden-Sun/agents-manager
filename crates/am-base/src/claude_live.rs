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
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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
        // macOS 的字頭是 `⏺`，Linux 是 `●`（2.1.287 真畫面）。
        if line.starts_with(['⏺', '●']) {
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

#[cfg(any(test, feature = "test-hooks"))]
pub fn has_baseline(run_id: &str) -> bool {
    baselines().lock().unwrap_or_else(|e| e.into_inner()).contains_key(run_id)
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn is_fresh(run_id: &str) -> bool {
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
pub async fn adopt_statusline_model(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db), run: &db::Run, payload: &serde_json::Value) {
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
    .execute(app.db())
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
pub async fn observe(app: &(impl crate::capabilities::BotLocks + crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes), run: &db::Run, screen: &str) {
    let seen = parse(screen);
    if seen == Switch::default() {
        return;
    }
    // 鎖外那份畫面可能是網頁套用前的確認行。先拿 bot 鎖再重讀，寫進去的才是套用之後的畫面。
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(fresh) = client.pane_read(run.pane_id.as_deref().unwrap_or(""), "visible", 80).await else { return };
    let seen = parse(&fresh.text);
    if seen == Switch::default() {
        return;
    }
    let Ok(Some(bot)) = db::bot(app.db(), &run.bot_id).await else { return };
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
    let Ok(Some(run)) = db::run(app.db(), &run.id).await else { return };
    // run 的落差（跟 runtime 比）與 bot 設定的落差（跟 bots.* 比）分開算（#743）：bot 的 UPDATE 失敗時 runtime 已經對了，
    // 下一輪只剩 bot 落後，仍要補寫，不能因為 runtime 已收斂就當作完成。
    // 只跟 run 自己記的比，不退回 `bots.*`：child 的模型跟設定一樣、強度不一樣時，以前只寫了 runtime_effort，
    // runtime_model 留空——網頁認定 runtime 已知、模型卻是空的，畫成「CLI 預設 ⟳」（2026-10-02 使用者：cf-優化 的 nv-opus／nv-fable）。
    let cur_model = run.runtime_model.clone();
    let cur_effort = run.runtime_effort.clone();
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
            .execute(app.db())
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
            .execute(app.db())
            .await;
        match wrote {
            Ok(_) => {
                changed = true;
                // `bot_status` 不帶設定，網頁收到 `bot_changed` 才重抓 bots（不然 runtime 跟設定一樣了還畫著 ⟳）。
                app.emit("bot_changed", serde_json::json!({"bot_id": bot.id})).await;
            }
            Err(e) => tracing::warn!(run = %run.id, bot = %bot.name, error = %e, "could not follow the claude runtime switch in bots.model/effort, retrying next sweep"),
        }
    }
    if changed {
        app.emit_bot_status(&bot.id).await;
    }
}
