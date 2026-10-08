//! `[judge]`：撞限偵測的第二意見，**只記錄、不改行為**（issue #240，SPEC §4.3c）。
//!
//! 畫面比對判定「這是新的撞限」的那一刻，另外問 TypeSafe 的 Jev 一題是非題：那一行是介面自己畫的，
//! 還是 bot 印出來的內容（原始碼、diff、測試輸出——#227／#237 誤判的來源）。兩邊的答案寫進
//! `judge_shadow`，之後拿 `cleared_at`（撞限多久之後就被成功回合清掉）對帳。回合照收、額度照標，
//! 這個模組的任何失敗都不影響它們。
//!
//! 畫面會離開這台機器，所以閘門有兩層、預設全關：`[judge] enabled` 與 `projects` 名單；送出前逐行
//! 遮罩（[`mask`]）。key 只存檔案路徑，每次呼叫才讀，不進 DB／log／API。

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::config::JudgeCfg;

#[path = "judge/collision.rs"]
pub mod collision;
#[path = "judge/report.rs"]
pub mod report;
#[path = "judge/stuck.rs"]
pub mod stuck;

const TAIL_LINES: usize = 60;
const TAIL_CHARS: usize = 6000;
const REDACTED: &str = "‹redacted›";

/// 畫面比對命中的那一刻複製出來的東西；呼叫端握著 bot 鎖，所以只複製、不在鎖內問。
pub struct Sample {
    pub bot_id: String,
    pub run_id: String,
    pub project_id: String,
    pub kind: String,
    pub matched_line: String,
    pub screen: String,
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS judge_shadow (
           id TEXT PRIMARY KEY,
           at TEXT NOT NULL,
           bot_id TEXT NOT NULL,
           run_id TEXT NOT NULL,
           kind TEXT NOT NULL,
           -- 遮罩後的命中行；畫面全文不存（要複查看 runs.turn_error 釘的快照）。
           matched_line TEXT NOT NULL,
           composer_idle INTEGER NOT NULL,
           regex_verdict TEXT NOT NULL,
           jev_is_live_ui REAL,
           -- #557 撞題 Noul。不塞進 jev_is_live_ui（那個欄位只表示「是不是活的介面」）。
           jev_same_work REAL,
           model TEXT,
           ms INTEGER,
           input_tokens INTEGER,
           error TEXT,
           -- 這顆 bot 的撞限之後被成功回合清掉的時刻：撞限後幾分鐘內就清掉＝當時其實沒撞限。
           cleared_at TEXT,
           -- #558：完成回報對到哪一筆交辦。機率不塞進 jev_is_live_ui（那個欄位只表示「是不是活的介面」）。
           assignment_id TEXT,
           claims_verified REAL,
           asks_parent_action REAL
         )",
    )
    .execute(pool)
    .await?;
    // 舊庫沒有這一欄。全新庫的 CREATE 已經有，這裡是 no-op。
    if !crate::db::has_column(pool, "judge_shadow", "jev_same_work").await? {
        sqlx::query("ALTER TABLE judge_shadow ADD COLUMN jev_same_work REAL").execute(pool).await?;
    }
    // 既有 DB 的 CREATE TABLE IF NOT EXISTS 不會補欄。
    for (col, ty) in [("assignment_id", "TEXT"), ("claims_verified", "REAL"), ("asks_parent_action", "REAL")] {
        if !crate::db::has_column(pool, "judge_shadow", col).await? {
            sqlx::query(&format!("ALTER TABLE judge_shadow ADD COLUMN {col} {ty}")).execute(pool).await?;
        }
    }
    Ok(())
}

/// 為什麼這一筆不問。都不寫帳本：關著的東西不該留痕跡。
#[derive(Debug, PartialEq)]
pub enum Skip {
    Disabled,
    ProjectNotListed,
    Fuse,
}

pub fn gate(cfg: &JudgeCfg, project_id: &str, project_label: &str, asked_last_hour: i64) -> Option<Skip> {
    if !cfg.enabled {
        return Some(Skip::Disabled);
    }
    if !cfg.projects.iter().any(|p| p == project_id || p == project_label) {
        return Some(Skip::ProjectNotListed);
    }
    if asked_last_hour >= cfg.max_per_hour as i64 {
        return Some(Skip::Fuse);
    }
    None
}

/// **先占位再問**：數一次額度、通過就立刻寫一筆占位列，兩件事在同一把鎖裡做完（issue #481）。
///
/// 以前是「數 → 問 → 寫」，中間隔著一次外部 HTTP。`shadow_limit_hit` 是 `tokio::spawn` 出去的，
/// 所以 K 顆 bot 同時撞限時，K 個 task 會在第一筆 INSERT 落地之前讀到同一個數字、一起放行，
/// 保險絲就被衝破。
///
/// 鎖**不跨那次 HTTP**：裡面只有兩句快的 DB 操作，不然整條路會被序列化成一次一個。
/// 兩個呼叫端（`observe` 的撞限樣本、`stuck::inspect` 的卡住畫面）共用這一支——
/// 只有一邊占位的話，另一邊照樣衝得過去（i339 review #481）。
///
/// 占位列先寫 `error='pending'`，答案由呼叫端問完再 [`settle_slot`] 補上。
#[allow(clippy::too_many_arguments)]
pub async fn reserve_slot(
    app: &(impl crate::capabilities::Db + crate::judge::JudgeFuse),
    cfg: &JudgeCfg,
    project_id: &str,
    project_label: &str,
    bot_id: &str,
    run_id: &str,
    kind: &str,
    matched_line: &str,
    composer_idle: bool,
    regex_verdict: &str,
) -> Result<String> {
    let id = crate::db::ulid();
    let _g = app.judge_fuse().lock().await;
    let asked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE at >= ?")
        .bind(crate::db::iso_in(-3600))
        .fetch_one(app.db())
        .await?;
    if let Some(skip) = gate(cfg, project_id, project_label, asked) {
        return Err(anyhow!("{skip:?}"));
    }
    sqlx::query(
        "INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(&id)
    .bind(crate::db::now())
    .bind(bot_id)
    .bind(run_id)
    .bind(kind)
    .bind(matched_line)
    .bind(composer_idle)
    .bind(regex_verdict)
    .execute(app.db())
    .await?;
    Ok(id)
}

/// 把 [`reserve_slot`] 占下的那一列補成終態。**失敗也留著**、不退還名額：逾時／5xx 也是真的
/// 送出去過一次（花費與「畫面離開這台機器」都已經發生）；失敗就退名額的話，一個壞掉的端點
/// 會讓保險絲永遠跳不了——那正好是最需要它跳的時候。
pub async fn settle_slot(
    app: &impl crate::capabilities::Db,
    id: &str,
    p: Option<f64>,
    model: Option<String>,
    ms: i64,
    tokens: Option<i64>,
    error: Option<String>,
) -> Result<()> {
    sqlx::query("UPDATE judge_shadow SET jev_is_live_ui=?, model=?, ms=?, input_tokens=?, error=? WHERE id=?")
        .bind(p)
        .bind(model)
        .bind(ms)
        .bind(tokens)
        .bind(error)
        .bind(id)
        .execute(app.db())
        .await?;
    Ok(())
}

/// 開機收尾：上一輪 daemon 在「占位」與「補答案」之間被收掉時留下的 `pending` 列（i339 review #481）。
///
/// 不刪掉——那一次很可能真的送出去過（花費與畫面外流已經發生），刪了帳本就對不上。
/// 改成一個**終態**：`error='interrupted'`，帳本與 `GET /api/judge/shadow` 上看得出來是
/// 「daemon 中途被收掉、答案沒回來」，而不是永遠顯示「在飛」。
pub async fn settle_interrupted(app: &impl crate::capabilities::Db) -> Result<u64> {
    let n = sqlx::query("UPDATE judge_shadow SET error='interrupted' WHERE error='pending'")
        .execute(app.db())
        .await?
        .rows_affected();
    if n > 0 {
        tracing::info!(rows = n, "judge shadow：上一輪留下的占位列收成 interrupted");
    }
    Ok(n)
}

pub async fn observe(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::judge::JudgeFuse), s: Sample) -> Result<()> {
    let cfg = app.cfg().get().await.judge;
    if !cfg.enabled {
        return Ok(());
    }
    let label = crate::db::project(app.db(), &s.project_id).await?.map(|p| p.label).unwrap_or_default();
    let lines: Vec<&str> = s.screen.lines().collect();
    let composer_idle = crate::tui_prompts::composer_is_idle(&lines);
    let matched = mask(&s.matched_line);
    let body = request_body(&cfg.model, &s.kind, &matched, &tail(&s.screen), composer_idle);

    let id = reserve_slot(app, &cfg, &s.project_id, &label, &s.bot_id, &s.run_id, &s.kind, &matched, composer_idle, "limit_hit").await?;

    let started = Instant::now();
    let answer = match read_key(&cfg.key_file) {
        Ok(key) => ask(&cfg, &key, &body).await,
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    let (p, model, tokens, error) = match answer {
        Ok(a) => (Some(a.is_live_ui), a.model, a.input_tokens, None),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    settle_slot(app, &id, p, model, ms, tokens, error).await?;
    Ok(())
}

/// 多久以前的撞限還算「這一次清掉的」（issue #453）。5 小時窗撞上去之後，下一次成功回合通常就在幾小時內，
/// 24 小時已經很寬；再舊的讓它維持 NULL——那是「從未被清掉」，不是「N 天後才清掉」。
const CLEAR_WINDOW_HOURS: i64 = 24;

/// 週限用的下界（issue #453 的跟進審核）。**24 小時涵蓋不了週限**：撞了週限之後可能好幾天才有下一次
/// 成功回合，用同一個下界會讓「真的撞週限又恢復」跟「那顆 bot 就此沒再跑過」在帳本上長得一模一樣，
/// 之後照 `cleared_at IS NOT NULL` 篩樣本會把週限整批篩掉。窗本身是 7 天，多給一天涵蓋重置前後的誤差。
const WEEKLY_CLEAR_WINDOW_HOURS: i64 = 24 * 8;

/// 這一筆撞的是不是週限。`judge_shadow` 沒有窗別欄位，但存著命中的那一行（`matched_line`），
/// 而講週限的橫幅一定帶得出「weekly」／「7-day」這類字（`lifecycle::screen::grok_limit_window` 用的是
/// 同一個訊號）。認不出來就當 5 小時窗——寧可少蓋一筆，也不要把久遠的舊帳追認成剛剛清掉。
fn weekly_limit_line(matched_line: &str) -> bool {
    let low = matched_line.to_ascii_lowercase();
    ["weekly", "7-day", "7 day", "seven day", "seven-day", "per week"].iter().any(|k| low.contains(k))
}

/// 成功回合清掉這顆 bot 的撞限時呼叫：把還沒對帳的那幾筆蓋上時刻。失敗只記 log。
///
/// 只蓋**撞限**那一類、而且是**近期**的（issue #453）：
/// * `regex_verdict = 'limit_hit'`——`cleared_at` 的語意（「撞限之後被成功回合清掉」）只對撞限成立。
///   `stuck_queued`（`judge/stuck.rs`）講的是「畫面底部有個不認得的框」，跟撞限無關，以前會被一起蓋上時刻，
///   帳本因此多出「某筆 stuck_queued 在某時被清掉」這種沒有意義的資料，照 `cleared_at` 篩樣本也會混進來。
/// * `at >= now - [`CLEAR_WINDOW_HOURS`]`——一筆撞限如果那顆 bot 之後一直沒有成功回合（停用、軟刪、
///   額度真的沒回來），它本來就該停在 NULL。不設下界的話，幾天後那顆 bot 被 resume 的那一次清除，
///   會把這筆舊的追認成「剛剛清掉」，把「從未被清掉」這個事實改寫掉。
pub async fn note_cleared(pool: &SqlitePool, bot_id: &str) {
    let now = chrono::Utc::now();
    // 先撈最寬的窗，再逐筆按「撞的是哪個窗」決定算不算數：下界寫在 Rust，不必把橫幅的字串搬進 SQL。
    let widest = crate::db::iso_at(now - chrono::Duration::hours(WEEKLY_CLEAR_WINDOW_HOURS));
    let rows: Vec<(String, String, String)> = match sqlx::query_as(
        "SELECT id, at, matched_line FROM judge_shadow
          WHERE bot_id = ? AND cleared_at IS NULL AND regex_verdict = 'limit_hit' AND at >= ?",
    )
    .bind(bot_id)
    .bind(&widest)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(bot = %bot_id, error = %e, "judge shadow rows not settled");
            return;
        }
    };
    let cutoff = crate::db::iso_at(now - chrono::Duration::hours(CLEAR_WINDOW_HOURS));
    let at_now = crate::db::now();
    for (id, at, matched_line) in rows {
        if at < cutoff && !weekly_limit_line(&matched_line) {
            continue;
        }
        if let Err(e) = sqlx::query("UPDATE judge_shadow SET cleared_at = ? WHERE id = ? AND cleared_at IS NULL")
            .bind(&at_now)
            .bind(&id)
            .execute(pool)
            .await
        {
            tracing::debug!(bot = %bot_id, row = %id, error = %e, "judge shadow row not settled");
        }
    }
}

struct Answer {
    is_live_ui: f64,
    model: Option<String>,
    input_tokens: Option<i64>,
}

/// 網頁「環境設定」貼進來的 key：寫到 `key_file`，權限 600（目錄 700）。先寫暫存檔再 rename，半截的檔不會被讀到。
/// key 只落在這個檔裡——不進 config.toml、DB、log，也不會從任何 API 回出去。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub fn write_key(path: &str, key: &str) -> Result<()> {
    stage_key(path, key)?.publish()
}

/// 已寫好、fsync 過、還沒生效的新 key（0600 暫存檔）。`publish` 才原子換掉現行的 key 檔；
/// 沒 publish 就 drop 會把暫存檔刪掉。設定頁「token＋開關」一起存時，先暫存、config 寫成功才 publish，
/// config 失敗就不會留下一把已經生效的新 key（issue #342）。
pub struct StagedKey {
    tmp: std::path::PathBuf,
    dest: std::path::PathBuf,
    done: bool,
}

impl StagedKey {
    /// 暫存檔路徑，給 `key_status` 之類的檢查用（不含 key 內容）。
    pub fn staged_path(&self) -> &std::path::Path {
        &self.tmp
    }

    pub fn publish(mut self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.dest)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for StagedKey {
    fn drop(&mut self) {
        if !self.done {
            std::fs::remove_file(&self.tmp).ok();
        }
    }
}

pub fn stage_key(path: &str, key: &str) -> Result<StagedKey> {
    let key = key.trim();
    if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!("key must be one line without spaces"));
    }
    let path = std::path::PathBuf::from(crate::config::expand_home(path, &std::env::var("HOME").unwrap_or_default()));
    let dir = path.parent().ok_or_else(|| anyhow!("key_file has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", crate::db::ulid()));
    let written = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(key.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    Ok(StagedKey { tmp, dest: path, done: false })
}

/// key 檔現在能不能用；不能用的原因（不含 key 本身）。
pub fn key_status(path: &str) -> std::result::Result<(), String> {
    read_key(path).map(|_| ()).map_err(|e| e.to_string())
}

/// 檔案權限對 group／other 開著就拒用：key 不該是別人讀得到的。
pub fn read_key(path: &str) -> Result<String> {
    let path = crate::config::expand_home(path, &std::env::var("HOME").unwrap_or_default());
    let meta = std::fs::metadata(&path).map_err(|e| anyhow!("key file unreadable: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(anyhow!("key file is readable by others; chmod 600 it"));
        }
    }
    let _ = &meta;
    let key = std::fs::read_to_string(&path).map_err(|e| anyhow!("key file unreadable: {e}"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("key file is empty"));
    }
    Ok(key.to_string())
}

/// 給 `stuck` 用的通用回答：某一題 noul 的機率。
pub struct NoulAnswer {
    pub value: f64,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
}

/// 打一次 System One。逾時、429、5xx 都只記一筆 error，不重試——shadow 少一筆無所謂，不值得佔連線。
/// 撞限、卡住的畫面、回報旗標都走這支，不另寫客戶端。
pub async fn post_systemone(cfg: &JudgeCfg, key: &str, body: &Value) -> Result<Value> {
    let client = reqwest::Client::builder().timeout(Duration::from_millis(cfg.timeout_ms)).build()?;
    let resp = client.post(&cfg.endpoint).bearer_auth(key).json(body).send().await.map_err(|e| anyhow!("request failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("http {}", status.as_u16()));
    }
    resp.json().await.map_err(|e| anyhow!("response was not json: {e}"))
}

/// 一次就好：逾時、429、5xx 都只記一筆 error，不重試。
pub async fn ask_noul(cfg: &JudgeCfg, key: &str, body: &Value, question: &str) -> Result<NoulAnswer> {
    let v = post_systemone(cfg, key, body).await?;
    let value = v["answers"][question]["noul"].as_f64().ok_or_else(|| anyhow!("no noul in the answer"))?;
    Ok(NoulAnswer { value, model: v["model"].as_str().map(str::to_string), input_tokens: v["usage"]["input_tokens"].as_i64() })
}

async fn ask(cfg: &JudgeCfg, key: &str, body: &Value) -> Result<Answer> {
    let a = ask_noul(cfg, key, body, "is_live_ui").await?;
    Ok(Answer { is_live_ui: a.value, model: a.model, input_tokens: a.input_tokens })
}

/// 題目照 spike（`reports/jev-spike/build_cases_b.py`）量過的那一版，多一個 `composer_idle`。用真的 Jev 重跑
/// 同一批 26 題：25 對、真撞限 5 題都 ≥0.75。答錯的仍是「確認框留在上方、輸入列已空」那一題（0.90）——
/// 給了 `composer_idle` 它也不看，所以這個欄位主要是留在帳本裡給事後分組用，不是指望 Jev 用它。
fn request_body(model: &str, kind: &str, matched: &str, screen: &str, composer_idle: bool) -> Value {
    json!({
        "model": model,
        "state": {"agent": kind, "needle": matched, "composer_idle": composer_idle, "screen": screen},
        "questions": {"is_live_ui": {
            "type": "noul",
            "instructions": {
                "question": "Is `needle` on this screen a live element drawn by the agent program's own interface that currently applies to the user?",
                "focus": "Tell interface elements apart from content that merely contains the same words: source code, a diff, grep or cat output, test failure messages, an issue body, or the agent's own prose quoting the text. `needle` is a usage-limit notice. `composer_idle` true only means no modal dialog is blocking the input box; a usage-limit notice can still be live right above an idle input box."
            },
            "criteria": {
                "true": "The interface itself is showing this notice, menu or dialog right now",
                "false": "The words appear inside printed or quoted content; the interface is not in that state"
            }
        }}
    })
}

pub fn tail(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let from = lines.len().saturating_sub(TAIL_LINES);
    let mut out = mask(&lines[from..].join("\n"));
    if out.chars().count() > TAIL_CHARS {
        out = out.chars().rev().take(TAIL_CHARS).collect::<Vec<_>>().into_iter().rev().collect();
    }
    out
}

/// 逐行、逐個空白分隔的字遮罩。盡力而為——真正的閘門是 `[judge]` 的兩層開關，不是這裡。
pub fn mask(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_pem = false;
    for line in text.lines() {
        if line.contains("-----BEGIN ") {
            in_pem = true;
        }
        if in_pem {
            if line.contains("-----END ") {
                in_pem = false;
                out.push(REDACTED.to_string());
            }
            continue;
        }
        out.push(mask_line(line));
    }
    if in_pem {
        out.push(REDACTED.to_string());
    }
    out.join("\n")
}

fn mask_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    // 前一個字是 `Bearer`，或是以 `=`／`:` 結尾的憑據名（`password: hunter2`）→ 這個字整個遮掉。
    let mut secret_next = false;
    // 保留原本的空白：畫面的排版本身就是 Jev 判斷的依據。
    for piece in split_keep_ws(line) {
        if piece.chars().all(char::is_whitespace) {
            out.push_str(piece);
            continue;
        }
        // 判斷要看**去掉包裹標點**之後的字（issue #451）：JSON 寫成 `"Bearer`、`{"Authorization":`，
        // 用原字比 `== "BEARER"` 永遠不成立，於是後面那條 JWT 原樣送出去。
        let upper = bare(piece).to_ascii_uppercase();
        // `Authorization: Bearer <token>`：`Authorization:` 讓 `secret_next` 成立，但要遮的是 token，
        // 不是 `Bearer` 這個字——認證方式留著，版面才看得出這行是什麼（Jev 判的就是版面）。
        let scheme = matches!(upper.as_str(), "BEARER" | "BASIC" | "DIGEST" | "TOKEN");
        // 遮掉整個片段會把 JSON 的收尾一起吃掉（`"Bearer <jwt>"}` → 右邊的 `"}` 不見了）。
        // 立場跟「認證方式留著」一樣：版面是 Jev 判斷的依據，只換掉字本身（issue #451 的跟進審核）。
        let masked = if secret_next && !scheme {
            let (lead, trail) = wrapping(piece);
            format!("{lead}{REDACTED}{trail}")
        } else {
            mask_word(piece)
        };
        // `Bearer`／`Basic`／`Digest`／`Token` 自己就點得起來：以前只有 `BEARER` 那一條，所以
        // `Authorization: Basic <b64>` 有遮（靠前面的 `Authorization:` 接力），裸寫的 `Basic <b64>` 沒遮。
        secret_next = scheme || (upper.ends_with(['=', ':']) && SECRET_NAMES.iter().any(|n| upper.contains(n)));
        out.push_str(&masked);
    }
    out
}

fn split_keep_ws(line: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut ws = None;
    for (i, c) in line.char_indices() {
        let is_ws = c.is_whitespace();
        if ws.is_some_and(|w| w != is_ws) {
            pieces.push(&line[start..i]);
            start = i;
        }
        ws = Some(is_ws);
    }
    if start < line.len() {
        pieces.push(&line[start..]);
    }
    pieces
}

const KEY_PREFIXES: [&str; 9] = ["sk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxa-", "xoxp-", "AKIA"];
/// `AUTHORIZATION`／`COOKIE`／`CREDENTIAL` 是 issue #451 補的：JSON header 那一行的名字就是它們，
/// 名字認得出來，後面那個字（`"Bearer`、或直接是 token）才遮得掉。`X-AM-Token` 已經被 `TOKEN` 蓋到。
const SECRET_NAMES: [&str; 8] = ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "AUTHORIZATION", "COOKIE", "CREDENTIAL"];

/// 去掉 JSON／程式碼常見的包裹標點。`mask_word` 與 `mask_line` 的 `secret_next` 要看同一個字，
/// 不然 `Bearer x` 遮得掉、`"Bearer", "x"` 遮不掉（issue #451）。
fn bare(word: &str) -> &str {
    word.trim_matches(WRAPPERS)
}

/// 這個片段前後各黏了哪些包裹標點。遮掉的時候把它們接回去，版面才不會缺一角。
fn wrapping(word: &str) -> (&str, &str) {
    let lead = word.len() - word.trim_start_matches(WRAPPERS).len();
    let trail = word.len() - word.trim_end_matches(WRAPPERS).len();
    // 整個片段都是標點時不要重疊切。
    if lead + trail >= word.len() {
        return (word, "");
    }
    (&word[..lead], &word[word.len() - trail..])
}

/// 包裹標點：原本寫在 `mask_word` 的 `trim_matches` 裡（`"'`,;()[]{}<>`），抽出來給 `bare`／`wrapping` 共用。
const WRAPPERS: [char; 13] = ['"', '\'', '`', ',', ';', '(', ')', '[', ']', '{', '}', '<', '>'];

fn mask_word(word: &str) -> String {
    let bare = bare(word);
    if KEY_PREFIXES.iter().any(|p| bare.starts_with(p)) && bare.len() >= 12 {
        return REDACTED.to_string();
    }
    // NAME=value／NAME: value 連在一起的那種；分開寫的靠下面的高熵規則。
    if let Some(i) = bare.find(['=', ':']) {
        let (name, value) = (bare[..i].to_ascii_uppercase(), &bare[i + 1..]);
        if !value.is_empty() && SECRET_NAMES.iter().any(|n| name.contains(n)) {
            return format!("{}{}{REDACTED}", &bare[..i], &bare[i..=i]);
        }
    }
    // URL 的 query string：整串是同一個片段，上面那條只看**第一個** `=`／`:`，而 URL 的第一個冒號永遠是
    // scheme 的（`https:`），所以 `?token=…` 一路漏到底（issue #451 的跟進審核）。逐段看、只換值。
    if let Some(masked) = mask_query(bare) {
        // `bare` 已經去掉包裹標點，接回去才不會把 `curl '…'` 的引號吃掉。
        let (lead, trail) = wrapping(word);
        return format!("{lead}{masked}{trail}");
    }
    // `user:pass@host` 的憑據：以前要網域含 `.` 才遮，於是 `http://u:p@localhost:8080/x` 漏掉。
    // 有 `@`、而且 `@` 左邊有冒號（＝帶密碼的 userinfo）就遮，網域長什麼樣不管。
    let userinfo = bare.rsplit_once('@').is_some_and(|(left, _)| left.contains(':') && !left.is_empty());
    if bare.contains('@') && !bare.starts_with('@') && (userinfo || bare.rsplit('@').next().is_some_and(|d| d.contains('.'))) {
        return REDACTED.to_string();
    }
    if looks_random(bare) || looks_jwt(bare) {
        return REDACTED.to_string();
    }
    match home_prefix(word) {
        Some((i, n)) => {
            let rest = &word[i + n..];
            let end = rest.find('/').unwrap_or(rest.len());
            format!("{}~{}", &word[..i], &rest[end..])
        }
        None => word.to_string(),
    }
}

/// 家目錄前綴的位置與長度：macOS 的 `/Users/<名>`、Linux 的 `/home/<名>`（SPEC「Linux 主機」）。
/// `/home/` 也是常見的網址路徑（`https://x.com/home/…`），前一個字是網域或路徑段的一部分就不算。
fn home_prefix(word: &str) -> Option<(usize, usize)> {
    let users = word.find("/Users/").map(|i| (i, "/Users/".len()));
    let home = word.match_indices("/home/").map(|(i, _)| i).find(|&i| {
        !word[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    });
    let home = home.map(|i| (i, "/home/".len()));
    [users, home].into_iter().flatten().min()
}

/// query string 裡的憑據：`https://h/api?token=abc&page=2` → 只把 `token` 的值換掉，其餘原樣。
/// `None`＝這個字裡沒有該遮的 query 參數（呼叫端接著走原本的規則）。
///
/// 值的名字比 [`SECRET_NAMES`] 多認幾個常見的短名：query string 裡 `key`／`auth`／`sig` 就是憑據，
/// 但把它們放進 `SECRET_NAMES` 會讓 `monkey=`、`sig:` 這種一般的字也中招，所以只在這裡認。
fn mask_query(word: &str) -> Option<String> {
    let (head, query) = word.split_once('?')?;
    let mut out = String::with_capacity(word.len());
    out.push_str(head);
    out.push('?');
    let mut changed = false;
    for (i, seg) in query.split('&').enumerate() {
        if i > 0 {
            out.push('&');
        }
        match seg.split_once('=') {
            Some((name, value)) if !value.is_empty() && query_secret(&name.to_ascii_uppercase()) => {
                out.push_str(name);
                out.push('=');
                out.push_str(REDACTED);
                changed = true;
            }
            _ => out.push_str(seg),
        }
    }
    changed.then_some(out)
}

fn query_secret(name: &str) -> bool {
    SECRET_NAMES.iter().any(|n| name.contains(n)) || matches!(name, "KEY" | "AUTH" | "SIG" | "SIGNATURE" | "CODE")
}

/// JWT：三段 base64url、用 `.` 分隔，而且第一段是 `eyJ`（JOSE header 的 `{"` base64url 過來一定長這樣）。
///
/// 獨立一條而不是把 `.` 加進 [`looks_random`] 的允許集合（issue #451）：那樣長路徑、版本字串、
/// 一整句話裡的長識別字都會開始中招，而遮掉版面正是 Jev 判斷不了的原因。簽章段允許是空的（`alg:none`）。
fn looks_jwt(s: &str) -> bool {
    let b64url = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let parts: Vec<&str> = s.split('.').collect();
    // 3 段＝JWS（簽章型），5 段＝JWE compact（header.key.iv.ciphertext.tag，中間幾段可能是空的）。
    // header 的開頭：`{"` → `eyJ`，`{ "`（大括號後有空白）→ `eyA`。
    matches!(parts.len(), 3 | 5)
        && (parts[0].starts_with("eyJ") || parts[0].starts_with("eyA"))
        && parts[0].len() >= 12
        && parts.iter().all(|p| b64url(p))
        && (parts.len() == 5 || parts[1].len() >= 8)
}

/// ≥32 個 base64／hex 字元、字母數字混雜、沒有路徑分隔以外的結構：當成憑據。git sha（40 hex）也會中，
/// 無妨——Jev 判斷的是版面，不需要 sha。
fn looks_random(s: &str) -> bool {
    if s.chars().count() < 32 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c)) {
        return false;
    }
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let letters = s.chars().filter(char::is_ascii_alphabetic).count();
    digits >= 4 && letters >= 4 && !s.contains("__") && s.matches('_').count() + s.matches('-').count() < 4
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "../../../daemon/src/judge_tests.rs"]
mod tests;

/// judge 的「一次只放一個影子判斷」保險絲。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait JudgeFuse: Send + Sync {
    fn judge_fuse(&self) -> &tokio::sync::Mutex<()>;
}
