//! `[judge]`：撞限偵測的第二意見，**只記錄、不改行為**（issue #240，SPEC §4.3c）。
//!
//! 畫面比對判定「這是新的撞限」的那一刻，另外問 TypeSafe 的 Jev 一題是非題：那一行是介面自己畫的，
//! 還是 bot 印出來的內容（原始碼、diff、測試輸出——#227／#237 誤判的來源）。兩邊的答案寫進
//! `judge_shadow`，之後拿 `cleared_at`（撞限多久之後就被成功回合清掉）對帳。回合照收、額度照標，
//! 這個模組的任何失敗都不影響它們。
//!
//! 畫面會離開這台機器，所以閘門有兩層、預設全關：`[judge] enabled` 與 `projects` 名單；送出前逐行
//! 遮罩（[`mask`]）。key 只存檔案路徑，每次呼叫才讀，不進 DB／log／API。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::config::JudgeCfg;
use crate::state::App;

pub mod http;
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
           model TEXT,
           ms INTEGER,
           input_tokens INTEGER,
           error TEXT,
           -- 這顆 bot 的撞限之後被成功回合清掉的時刻：撞限後幾分鐘內就清掉＝當時其實沒撞限。
           cleared_at TEXT
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 鎖外去問。關著的時候連 task 都不起。
pub fn shadow_limit_hit(app: &Arc<App>, sample: Sample) {
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = observe(&app, sample).await {
            tracing::debug!(error = %e, "judge shadow skipped");
        }
    });
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

pub async fn observe(app: &Arc<App>, s: Sample) -> Result<()> {
    let cfg = app.cfg.get().await.judge;
    if !cfg.enabled {
        return Ok(());
    }
    let label = crate::db::project(&app.db, &s.project_id).await?.map(|p| p.label).unwrap_or_default();
    let asked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE at >= ?")
        .bind(crate::db::iso_in(-3600))
        .fetch_one(&app.db)
        .await?;
    if let Some(skip) = gate(&cfg, &s.project_id, &label, asked) {
        return Err(anyhow!("{skip:?}"));
    }
    let lines: Vec<&str> = s.screen.lines().collect();
    let composer_idle = crate::tui_prompts::composer_is_idle(&lines);
    let matched = mask(&s.matched_line);
    let body = request_body(&cfg.model, &s.kind, &matched, &tail(&s.screen), composer_idle);

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
    sqlx::query(
        "INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict, jev_is_live_ui, model, ms, input_tokens, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'limit_hit', ?, ?, ?, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(crate::db::now())
    .bind(&s.bot_id)
    .bind(&s.run_id)
    .bind(&s.kind)
    .bind(&matched)
    .bind(composer_idle)
    .bind(p)
    .bind(model)
    .bind(ms)
    .bind(tokens)
    .bind(error)
    .execute(&app.db)
    .await?;
    Ok(())
}

/// 多久以前的撞限還算「這一次清掉的」（issue #453）。比 5 小時窗寬一截，涵蓋週限與 daemon 重啟後
/// 第一次成功回合；再舊的就讓它維持 NULL——那是「從未被清掉」，不是「N 天後才清掉」。
const CLEAR_WINDOW_HOURS: i64 = 24;

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
    let since = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(CLEAR_WINDOW_HOURS));
    let res = sqlx::query(
        "UPDATE judge_shadow SET cleared_at = ?
          WHERE bot_id = ? AND cleared_at IS NULL AND regex_verdict = 'limit_hit' AND at >= ?",
    )
    .bind(crate::db::now())
    .bind(bot_id)
    .bind(&since)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::debug!(bot = %bot_id, error = %e, "judge shadow rows not settled");
    }
}

struct Answer {
    is_live_ui: f64,
    model: Option<String>,
    input_tokens: Option<i64>,
}

/// 網頁「環境設定」貼進來的 key：寫到 `key_file`，權限 600（目錄 700）。先寫暫存檔再 rename，半截的檔不會被讀到。
/// key 只落在這個檔裡——不進 config.toml、DB、log，也不會從任何 API 回出去。
#[cfg(test)]
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
pub(crate) fn read_key(path: &str) -> Result<String> {
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
pub(crate) struct NoulAnswer {
    pub value: f64,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
}

/// 一次就好：逾時、429、5xx 都只記一筆 error，不重試——shadow 少一筆無所謂，不值得佔連線。
pub(crate) async fn ask_noul(cfg: &JudgeCfg, key: &str, body: &Value, question: &str) -> Result<NoulAnswer> {
    let client = reqwest::Client::builder().timeout(Duration::from_millis(cfg.timeout_ms)).build()?;
    let resp = client.post(&cfg.endpoint).bearer_auth(key).json(body).send().await.map_err(|e| anyhow!("request failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("http {}", status.as_u16()));
    }
    let v: Value = resp.json().await?;
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

pub(crate) fn tail(screen: &str) -> String {
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
    match word.find("/Users/") {
        Some(i) => {
            let rest = &word[i + "/Users/".len()..];
            let end = rest.find('/').unwrap_or(rest.len());
            format!("{}~{}", &word[..i], &rest[end..])
        }
        None => word.to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, projects: &[&str]) -> JudgeCfg {
        JudgeCfg { enabled, projects: projects.iter().map(|s| s.to_string()).collect(), ..JudgeCfg::default() }
    }

    #[test]
    fn both_switches_must_be_on_and_the_fuse_holds() {
        assert_eq!(gate(&JudgeCfg::default(), "P1", "agents-manager", 0), Some(Skip::Disabled));
        assert_eq!(gate(&cfg(true, &[]), "P1", "agents-manager", 0), Some(Skip::ProjectNotListed));
        assert_eq!(gate(&cfg(false, &["P1"]), "P1", "agents-manager", 0), Some(Skip::Disabled));
        assert_eq!(gate(&cfg(true, &["other"]), "P1", "agents-manager", 0), Some(Skip::ProjectNotListed));
        assert_eq!(gate(&cfg(true, &["P1"]), "P1", "x", 0), None);
        assert_eq!(gate(&cfg(true, &["agents-manager"]), "P1", "agents-manager", 59), None);
        assert_eq!(gate(&cfg(true, &["agents-manager"]), "P1", "agents-manager", 60), Some(Skip::Fuse));
    }

    #[test]
    fn secrets_are_masked_and_layout_is_kept() {
        let m = |s: &str| mask(s);
        assert_eq!(m("  export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789"), format!("  export GITHUB_TOKEN={REDACTED}"));
        assert_eq!(m("key sk-ant-REDACTED done"), format!("key {REDACTED} done"));
        assert_eq!(m("Authorization: Bearer abc.def.ghi"), format!("Authorization: Bearer {REDACTED}"));
        assert_eq!(m("db_password: hunter2 # x"), format!("db_password: {REDACTED} # x"));
        assert_eq!(m("DB_PASSWORD=hunter2"), format!("DB_PASSWORD={REDACTED}"));
        assert_eq!(m("mail someone@example.com now"), format!("mail {REDACTED} now"));
        assert_eq!(m("  gpt-5.6-luna xhigh · /Users/m4p/project/agents-manager"), "  gpt-5.6-luna xhigh · ~/project/agents-manager");
        assert_eq!(m("token 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"), format!("token {REDACTED}"));
        assert_eq!(m("a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz"), format!("a\n{REDACTED}\nz"));
        // 撞限橫幅與一般程式碼原樣通過——遮掉了 Jev 就沒東西可判。
        let banner = "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), or try again at 3:22 AM.";
        assert_eq!(m(banner), banner);
        let code = r#"    low.contains("you hit your weekly limit") || low.contains("you've hit your weekly limit")"#;
        assert_eq!(m(code), code);
        assert_eq!(m("@mention and a_long_snake_case_identifier_that_is_not_a_secret_0123"), "@mention and a_long_snake_case_identifier_that_is_not_a_secret_0123");
    }

    /// issue #451：JSON／引號形式的 header。`"Bearer` 前面黏著雙引號，原本 `== "BEARER"` 不成立，
    /// 於是 `secret_next` 是 false；JWT 又因為有 `.` 被 `looks_random` 的字元集擋在門外，整條原樣送出去。
    ///
    /// 這裡的字串全是假的（header 是 `{"alg":"HS256"}`、payload 是 `{"sub":"1"}`，簽章隨便湊的）。
    #[test]
    fn json_shaped_bearer_and_bare_jwts_are_masked() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmnopqrstuvwxyz012345";
        let line = format!(r#"{{"Authorization": "Bearer {jwt}"}}"#);
        let out = mask(&line);
        assert!(!out.contains("eyJzdWIiOiIxIn0"), "JSON 形式的 Bearer 後面那條也要遮掉：{out}");
        assert!(!out.contains(jwt), "{out}");
        // 認證方式本身留著：遮掉版面等於把 Jev 要判的東西一起拿走。
        assert!(out.contains("Bearer"), "{out}");

        // 沒有 Bearer、單獨出現的 JWT 也要遮（log、curl 的 -H 拆行、程式碼字面值都會這樣）。
        assert_eq!(mask(&format!("token={jwt}")), format!("token={REDACTED}"));
        assert_eq!(mask(jwt), REDACTED);
        assert_eq!(mask(&format!("  header: {jwt} ok")), format!("  header: {REDACTED} ok"));

        // 空白分隔的那一版（既有行為）不能退步。
        assert_eq!(mask(&format!("Authorization: Bearer {jwt}")), format!("Authorization: Bearer {REDACTED}"));

        // 名字認得出來、值直接接在後面時也遮：`"Authorization":` 去掉包裹標點才對得上。
        let out = mask(r#"{"authorization": "abcdefghijklmnopqrstuvwxyz0123456789"}"#);
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{out}");
        assert!(mask(r#"cookie: "sess_abcdefghijklmnopqrstuvwxyz01234567""#).contains(REDACTED));
        // `Token <值>`（GitHub 那種寫法）：方式留著、值遮掉。
        assert_eq!(mask("Authorization: Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Authorization: Token {REDACTED}"));
    }

    /// issue #451 的跟進審核（i407）：四個反向缺口。字串全是捏造的。
    #[test]
    fn bare_schemes_query_strings_userinfo_and_jwe_are_masked_too() {
        // 1) 裸寫的 Basic／Token／Digest：以前只有 `BEARER` 點得起 `secret_next`，
        //    所以 `Authorization: Basic X` 有遮（靠前面那個字接力），單獨一行的 `Basic X` 沒遮。
        assert_eq!(mask("Basic dXNlcjpwYXNzd29yZA=="), format!("Basic {REDACTED}"));
        assert_eq!(mask("Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Token {REDACTED}"));
        assert_eq!(mask("Digest username=admin"), format!("Digest {REDACTED}"));

        // 2) query string：整串 URL 是同一個片段，第一個冒號是 scheme 的，名字比對不到；
        //    `/`／`?`／`.` 又讓 looks_random 回 false。只換值、其餘原樣。
        assert_eq!(
            mask("https://h.example.invalid/api?token=abcdef123456&page=2"),
            format!("https://h.example.invalid/api?token={REDACTED}&page=2")
        );
        assert_eq!(mask("curl 'https://x.invalid/v1?api_key=k-abc123&q=1'"), format!("curl 'https://x.invalid/v1?api_key={REDACTED}&q=1'"));
        assert_eq!(mask("https://x.invalid/p?key=abc&sig=def"), format!("https://x.invalid/p?key={REDACTED}&sig={REDACTED}"));
        // 沒有憑據參數的 URL 一個字都不要動。
        let plain = "https://x.invalid/p?page=2&sort=name";
        assert_eq!(mask(plain), plain);

        // 3) `user:pass@`：以前要網域含 `.` 才遮。
        assert_eq!(mask("http://u:p@localhost:8080/path"), REDACTED);
        assert_eq!(mask("mail someone@example.com now"), format!("mail {REDACTED} now"));

        // 4) JWE compact 是五段。
        let jwe = "eyJhbGciOiJSU0EtT0FFUCJ9.abcdefgh.ijklmnop.qrstuvwxyz012345.tag12345";
        assert_eq!(mask(jwe), REDACTED);
        // `{ "alg"…`（大括號後有空白）的 header 是 `eyA` 開頭。
        assert_eq!(mask("eyAiYWxnIjoiSFMyNTYifQ.eyJzdWIiOiIxIn0.sig12345"), REDACTED);

        // 收尾標點留著：以前整個片段被換掉，JSON 的 `"}` 會不見。
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmnopqrstuvwxyz012345";
        let out = mask(&format!(r#"{{"Authorization": "Bearer {jwt}"}}"#));
        assert!(out.ends_with(r#""}"#), "版面要留著：{out}");
        assert!(!out.contains(jwt), "{out}");
    }

    /// 遮罩只能吃 token，不能吃版面：有 `.` 的一般字串（版本、路徑、檔名、句子）照樣原樣通過。
    /// 這條是 [`looks_jwt`] 的反面——當初沒有把 `.` 加進 `looks_random` 的允許集合就是為了這些。
    #[test]
    fn dotted_words_that_are_not_jwts_survive() {
        for s in [
            "gpt-5.6-luna",
            "v1.22.333-rc.1",
            "daemon/src/judge.rs:346",
            "web/src/api/index.ts",
            "eyJhbGci.short.x",
            "api.github.com",
            "0.93",
        ] {
            assert_eq!(mask(s), s, "{s} 不是 JWT，不該被遮掉");
        }
    }

    #[test]
    fn only_the_tail_of_the_screen_leaves_the_machine() {
        let screen: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let t = tail(&screen);
        assert!(t.starts_with("line 140"), "{}", &t[..20]);
        assert!(t.ends_with("line 199"));
        let wide: String = (0..60).map(|_| "x".repeat(500) + "\n").collect();
        assert_eq!(tail(&wide).chars().count(), TAIL_CHARS);
    }

    #[test]
    fn the_request_carries_no_bot_or_project_identity() {
        let body = request_body("jev-1.13.0", "grok", "You hit your weekly limit.", "screen", true);
        let state = body["state"].as_object().unwrap();
        let mut keys: Vec<_> = state.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["agent", "composer_idle", "needle", "screen"]);
        assert_eq!(body["model"], "jev-1.13.0");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, "k-test\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_key(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key(path.to_str().unwrap()).unwrap(), "k-test");
        assert!(read_key(dir.join("absent").to_str().unwrap()).unwrap_err().to_string().contains("unreadable"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 假的 Jev：數被打了幾次、記下收到的 body 與 Authorization。
    async fn fake_jev(status: u16) -> (String, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
        use axum::http::{HeaderMap, StatusCode};
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                log.lock().unwrap().push((auth, body));
                (StatusCode::from_u16(status).unwrap(), axum::Json(json!({"model": "jev-1.13.0", "answers": {"is_live_ui": {"type": "noul", "noul": 0.07}}, "usage": {"input_tokens": 812}})))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    async fn app_with(enabled: bool, projects: &[&str], endpoint: &str) -> (Arc<App>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test-0001\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let db = crate::db::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let judge = JudgeCfg { key_file: key.to_string_lossy().into_owned(), endpoint: endpoint.into(), ..cfg(enabled, projects) };
        store
            .update(move |c| {
                c.judge = judge;
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn sample() -> Sample {
        Sample {
            bot_id: "B1".into(),
            run_id: "R1".into(),
            project_id: "P1".into(),
            kind: "grok".into(),
            matched_line: "low.contains(\"you hit your weekly limit\") // ghp_abcdefghijklmnopqrstuvwxyz0123456789".into(),
            screen: "  ◆ Run: rg weekly /Users/m4p/project\n    screen.rs:458: low.contains(\"you hit your weekly limit\")\n❯\n".into(),
        }
    }

    async fn rows(app: &Arc<App>) -> Vec<(String, Option<f64>, Option<String>, Option<String>, bool)> {
        sqlx::query_as("SELECT matched_line, jev_is_live_ui, error, cleared_at, composer_idle FROM judge_shadow").fetch_all(&app.db).await.unwrap()
    }

    #[tokio::test]
    async fn an_answer_is_recorded_and_later_settled() {
        let (url, seen) = fake_jev(200).await;
        let (app, dir) = app_with(true, &["P1"], &url).await;
        observe(&app, sample()).await.unwrap();
        let got = rows(&app).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, Some(0.07));
        assert!(got[0].2.is_none() && got[0].3.is_none() && got[0].4, "{got:?}");
        assert!(!got[0].0.contains("ghp_"), "命中行存進帳本前也要遮罩：{}", got[0].0);
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Bearer k-test-0001");
        let sent = calls[0].1.to_string();
        assert!(!sent.contains("ghp_") && !sent.contains("/Users/m4p") && !sent.contains("B1") && !sent.contains("P1"), "{sent}");
        assert!(!app.cfg.get().await.judge.key_file.contains("k-test"), "設定裡只有路徑");

        note_cleared(&app.db, "other").await;
        assert!(rows(&app).await[0].3.is_none());
        note_cleared(&app.db, "B1").await;
        assert!(rows(&app).await[0].3.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// issue #453：`cleared_at` 的語意是「**撞限**之後被成功回合清掉」，所以那句 UPDATE 只能蓋
    /// 撞限那一類、而且是近期的。以前沒有任何條件，於是一次成功回合會順手：
    /// (1) 把 `stuck_queued`（跟撞限無關）也蓋上時刻；(2) 把幾天前那筆「從未被清掉」追認成「剛剛清掉」。
    #[tokio::test]
    async fn only_recent_limit_hits_are_settled_by_a_clear() {
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:1/x").await;
        let put = |id: &str, verdict: &str, at: String| {
            let (app, id, verdict) = (app.clone(), id.to_string(), verdict.to_string());
            async move {
                sqlx::query(
                    "INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict)
                     VALUES (?, ?, 'B1', 'R1', 'claude', 'x', 1, ?)",
                )
                .bind(&id)
                .bind(&at)
                .bind(&verdict)
                .execute(&app.db)
                .await
                .unwrap();
            }
        };
        let cleared = |id: &str| {
            let (app, id) = (app.clone(), id.to_string());
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT cleared_at FROM judge_shadow WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&app.db)
                    .await
                    .unwrap()
            }
        };
        let hours_ago = |h: i64| crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(h));

        put("fresh", "limit_hit", hours_ago(1)).await;
        put("stale", "limit_hit", hours_ago(CLEAR_WINDOW_HOURS + 1)).await;
        put("stuck", "stuck_queued", hours_ago(1)).await;

        note_cleared(&app.db, "B1").await;

        assert!(cleared("fresh").await.is_some(), "近期的撞限才是這次清掉的那一筆");
        assert!(cleared("stale").await.is_none(), "幾天前沒被清掉的撞限要維持 NULL，不是追認成剛剛清掉");
        assert!(cleared("stuck").await.is_none(), "stuck_queued 跟撞限無關，cleared_at 對它沒有意義");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn nothing_leaves_the_machine_unless_both_switches_are_on() {
        let (url, seen) = fake_jev(200).await;
        for (enabled, projects) in [(false, vec!["P1"]), (true, vec![]), (true, vec!["someone-else"])] {
            let (app, dir) = app_with(enabled, &projects, &url).await;
            let _ = observe(&app, sample()).await;
            assert!(rows(&app).await.is_empty());
            std::fs::remove_dir_all(&dir).ok();
        }
        assert!(seen.lock().unwrap().is_empty(), "關著還是打了 API");
    }

    #[tokio::test]
    async fn a_failing_service_is_one_error_row_and_no_retry() {
        let (url, seen) = fake_jev(429).await;
        let (app, dir) = app_with(true, &["P1"], &url).await;
        observe(&app, sample()).await.unwrap();
        let got = rows(&app).await;
        assert_eq!((got[0].1, got[0].2.as_deref()), (None, Some("http 429")));
        assert_eq!(seen.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_pasted_key_lands_in_a_600_file_and_nowhere_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid()));
        let path = dir.join("nested").join("api-key");
        let p = path.to_str().unwrap();
        assert!(key_status(p).unwrap_err().contains("unreadable"));
        write_key(p, "  k-test-0002\n").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_key(p).unwrap(), "k-test-0002");
        // 覆寫舊的；壞輸入不動既有的檔。
        write_key(p, "k-test-0003").unwrap();
        assert!(write_key(p, "two words").is_err() && write_key(p, " ").is_err());
        assert_eq!(read_key(p).unwrap(), "k-test-0003");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "暫存檔沒收乾淨");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_settings_page_can_switch_it_on_only_with_a_key_and_never_reads_the_key_back() {
        use axum::extract::State;
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:9/unused").await;
        std::fs::remove_file(dir.join("key")).unwrap();
        let put = |enabled, projects: Option<Vec<&str>>, token: Option<&str>| {
            let body = http::SettingsBody { enabled, projects: projects.map(|p| p.iter().map(|s| s.to_string()).collect()), token: token.map(str::to_string) };
            http::put_settings(State(app.clone()), axum::Json(body))
        };
        let refused = put(Some(true), None, None).await;
        assert!(matches!(refused, Err(crate::lifecycle::LcError::Conflict(ref v)) if v["error"] == "needs_key"), "沒 key 不給開");
        assert!(!app.cfg.get().await.judge.enabled);

        let saved = put(Some(true), Some(vec![" agents-manager ", "agents-manager", ""]), Some("k-test-0004")).await.unwrap().0;
        assert_eq!(saved["enabled"], true);
        assert_eq!(saved["projects"], json!(["agents-manager"]));
        assert_eq!(saved["key_present"], true);
        assert!(!saved.to_string().contains("k-test-0004"));
        assert!(!http::get_settings(State(app.clone())).await.0.to_string().contains("k-test-0004"));
        assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("k-test-0004"), "key 不進 config.toml");

        // 空 token＝不動現有的 key；只關開關。
        let off = put(Some(false), None, Some("  ")).await.unwrap().0;
        assert_eq!((off["enabled"].clone(), off["key_present"].clone()), (json!(false), json!(true)));
        assert_eq!(read_key(dir.join("key").to_str().unwrap()).unwrap(), "k-test-0004");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_config_update_leaves_the_old_key_live_and_no_staged_file() {
        use axum::extract::State;
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:9/unused").await;
        let key = dir.join("key");
        let keys = key.to_str().unwrap();
        assert_eq!(read_key(keys).unwrap(), "k-test-0001");
        // config.toml 被外面換成壞檔：ConfigStore::update 會在重讀時失敗
        std::fs::write(dir.join("config.toml"), "this is = = not toml").unwrap();
        // mtime 一定要跟 store 記的不同，否則不會重讀（時間解析度粗時會假綠／偶發紅）
        let f = std::fs::OpenOptions::new().write(true).open(dir.join("config.toml")).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
        let body = http::SettingsBody { enabled: Some(true), projects: Some(vec!["p".into()]), token: Some("k-test-0009".into()) };
        let err = http::put_settings(State(app.clone()), axum::Json(body)).await;
        assert!(matches!(err, Err(crate::lifecycle::LcError::Upstream(_))), "config 失敗要回錯");
        assert_eq!(read_key(keys).unwrap(), "k-test-0001", "config 失敗不可讓新 key 生效");
        assert_eq!(std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count(), 0, "暫存 key 要清掉");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_staged_key_is_inert_until_published() {
        let dir = std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid()));
        let path = dir.join("api-key");
        let p = path.to_str().unwrap();
        write_key(p, "k-old").unwrap();
        drop(stage_key(p, "k-new").unwrap());
        assert_eq!(read_key(p).unwrap(), "k-old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "沒 publish 的暫存檔要刪");
        assert!(stage_key(p, "two words").is_err());
        let st = stage_key(p, "k-new").unwrap();
        assert_eq!(read_key(p).unwrap(), "k-old");
        assert_eq!(read_key(&st.staged_path().to_string_lossy()).unwrap(), "k-new");
        st.publish().unwrap();
        assert_eq!(read_key(p).unwrap(), "k-new");
        std::fs::remove_dir_all(&dir).ok();
    }
}
