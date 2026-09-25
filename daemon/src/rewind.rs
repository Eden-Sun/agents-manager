//! 對話倒回（rewind，issue #405，SPEC §6.13）：選一則使用者訊息，把 claude bot 的對話倒回到**送出它之前**——
//! 那一則與之後的問答都不再在 context 裡，原文交回前端回填輸入框改寫。
//!
//! **驅動 TUI 自己的 `/rewind`**（使用者 2026-09-23 裁示；截斷 transcript 副本那版不採用，理由見 #405）：
//! 同一個 session、同一個 jsonl 內分支，session id 不變、不重啟，被倒掉的原文 CLI 會自己放回輸入列。
//! claude 2.1.280 的畫面（`rewind/claude_2.1.280_rewind_*.txt`，實機截的）：
//! 1. 輸入列打 `/rewind`＋Enter → 選單：由舊到新列出使用者訊息，最下面是 `❯ (current)`，只看得到兩三則，其餘寫成
//!    `↑ N more above`／`↓ N more below`。每則只顯示第一行（多行的後面加 `…`、太長的在欄寬截斷加 `…`）。
//! 2. 往上移到目標、Enter → 確認頁：`│ <原文>` 印出那一則（**太長的只印前 4 行或約 6 個折行，沒有任何截斷記號**），
//!    底下 `❯ 1. Restore conversation`／`2. Summarize from here`／`3. Summarize up to here`／`4. Never mind`。
//!    pane 矮的時候選項會被擠出畫面（14 列實測只剩 `The conversation will be forked.` 那兩行）。在確認頁按 Esc 回到選單，選單再 Esc 關掉。
//! 3. 按 `1`（選項是編號的：直接選 Restore，不管游標在哪、看不看得到選項）→ 對話停在那一則之前，原文出現在輸入列。
//!
//! 每一步都讀畫面確認才按下一步：選單真的開了、游標真的移動了、確認頁印的字真的是那一則。**確認頁的字對不上就 Esc
//! 退出、回錯，絕不在沒確認的情況下選 Restore。** 選到之後輸入列裡的原文由 daemon 用 ctrl+c 清掉（原文交給網頁的
//! 輸入框；留在 pane 裡的話，下一則打進去的 prompt 會接在它後面）。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::Json;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db;
use crate::lifecycle::{self, LcError, LcResult};
use crate::state::App;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

// ───────────── 時間（測試縮短） ─────────────

#[cfg(not(test))]
mod timing {
    pub const POLL_MS: u64 = 150;
    pub const TYPE_SETTLE_MS: u64 = 600;
    pub const MENU_WAIT_MS: u64 = 5_000;
    pub const STEP_WAIT_MS: u64 = 1_500;
    pub const CONFIRM_WAIT_MS: u64 = 3_000;
    pub const RESTORE_WAIT_MS: u64 = 6_000;
    pub const CLEAR_WAIT_MS: u64 = 2_000;
    pub const HINT_WAIT_MS: u64 = 5_000;
}
#[cfg(test)]
mod timing {
    pub const POLL_MS: u64 = 2;
    pub const TYPE_SETTLE_MS: u64 = 1;
    pub const MENU_WAIT_MS: u64 = 200;
    pub const STEP_WAIT_MS: u64 = 100;
    pub const CONFIRM_WAIT_MS: u64 = 200;
    pub const RESTORE_WAIT_MS: u64 = 200;
    pub const CLEAR_WAIT_MS: u64 = 100;
    pub const HINT_WAIT_MS: u64 = 200;
}
use timing::*;

/// claude 在 ctrl+c 清掉輸入列之後顯示的提示；顯示的這幾秒內再按一次 ctrl+c 會退出 claude。
const CTRL_C_HINT: &str = "Press Ctrl-C again to exit";
/// 往上最多移幾格：一段對話的使用者訊息再多也不會到這裡，到了就當找不到。
const MAX_STEPS: usize = 400;
/// 確認頁只印了原文的前一段（沒有截斷記號）時，要看起來**確實是被截斷的長度**才接受：至少這麼多行。
/// 2.1.280 實測截斷時一定有 4 行以上（多行的前 4 行、長的一行折成約 6 行）；比這短卻只對到前綴＝不是同一則。
const TRUNCATED_MIN_LINES: usize = 4;

// ───────────── 讀畫面（純函式） ─────────────

/// 選單的樣子。`selected` 是游標那一則的顯示文字，`None`＝在 `(current)`。`block` 用來判斷游標有沒有真的移動。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub selected: Option<String>,
    pub block: String,
}

/// 確認頁：`quoted` 是 `│` 那幾行（去掉 `(… ago)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub quoted: Vec<String>,
}

fn body(line: &str) -> &str {
    line.trim()
}

/// 最後一個 `Rewind` 標題以下的列；標題下面要有選單或確認頁才有的字（光一行 `Rewind` 可能是對話內容）。
fn rewind_region(screen: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = screen.lines().collect();
    let title = lines.iter().rposition(|l| body(l) == "Rewind")?;
    let below: Vec<&str> = lines[title + 1..].to_vec();
    let marked = below.iter().any(|l| {
        let b = body(l);
        b.starts_with("Enter to continue")
            || b.starts_with("Restore the code and/or conversation")
            || b.starts_with("⚠ No code restore")
            || b.starts_with("Confirm you want to restore")
    });
    marked.then_some(below)
}

/// 選單：最後一個 `Rewind` 標題以下有選單的字，而且看得到 `❯` 游標。
/// 標題上面的對話紀錄也有 `❯` 開頭的列（歷史 prompt），所以只看標題以下。pane 矮的時候底下的
/// `Enter to continue · Esc to cancel` 會被擠出畫面（2026-09-23 實機，14 列的 pane），不能靠它。
pub fn parse_menu(screen: &str) -> Option<Menu> {
    let below = rewind_region(screen)?;
    // 確認頁也在 `Rewind` 標題底下：有它的標題或選項就不是選單。
    if below.iter().any(|l| is_option_one(l) || body(l).starts_with("Confirm you want to restore")) {
        return None;
    }
    let cursor = below.iter().find_map(|l| body(l).strip_prefix('❯').map(|r| r.trim().to_string()))?;
    let selected = (cursor != "(current)").then_some(cursor);
    let block = below.iter().map(|l| body(l)).collect::<Vec<_>>().join("\n");
    Some(Menu { selected, block })
}

/// 看得到 rewind 的畫面（選單或確認頁，就算讀不全）：退出時用這個判斷還要不要按 Esc。
pub fn rewind_visible(screen: &str) -> bool {
    rewind_region(screen).is_some()
}

fn is_option_one(line: &str) -> bool {
    body(line).trim_start_matches('❯').trim().starts_with("1. Restore conversation")
}

/// 確認頁：標題 `Confirm you want to restore…` 或選項 `1. Restore conversation` 至少看得到一個（證明是這個對話框），
/// 以及說明 `The conversation will be forked.`（或選項）上面緊鄰的 `│` 區塊。pane 矮的時候選項會被擠出畫面、
/// 標題也可能被捲掉，兩個都看不到就不算；區塊上面被捲掉的話對到的只是後半段，比對自然會失敗。
pub fn parse_confirm(screen: &str) -> Option<Confirm> {
    let lines: Vec<&str> = screen.lines().collect();
    let anchor = lines.iter().rposition(|l| body(l) == "The conversation will be forked." || is_option_one(l))?;
    let titled = lines[..anchor].iter().any(|l| body(l).starts_with("Confirm you want to restore"));
    if !titled && !lines.iter().any(|l| is_option_one(l)) {
        return None;
    }
    let mut i = anchor;
    // 往上走到 `│` 區塊的最後一行（中間只能隔空行與說明，最多幾行）。
    while i > 0 && !body(lines[i - 1]).starts_with('│') {
        i -= 1;
        if anchor - i > 6 {
            return None;
        }
    }
    let end = i;
    while i > 0 && body(lines[i - 1]).starts_with('│') {
        i -= 1;
    }
    let mut quoted: Vec<String> = lines[i..end].iter().map(|l| body(l).trim_start_matches('│').trim().to_string()).collect();
    if quoted.last().is_some_and(|l| l.starts_with('(') && l.ends_with(" ago)")) {
        quoted.pop();
    }
    if quoted.is_empty() {
        return None;
    }
    Some(Confirm { quoted })
}

/// 選單或確認頁還開著。
pub fn in_rewind_ui(screen: &str) -> bool {
    parse_menu(screen).is_some() || parse_confirm(screen).is_some() || rewind_visible(screen)
}

/// 比對用：拿掉所有空白（折行、換行、縮排都不算）與圖片佔位 `[Image #N]`。
pub fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[Image #") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 8..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(']') {
            rest = &after[digits + 1..];
        } else {
            out.push_str("[Image #");
            rest = after;
        }
    }
    out.push_str(rest);
    out.chars().filter(|c| !c.is_whitespace()).collect()
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// 選單上的一則是不是目標：顯示的是第一行；尾巴有 `…` 表示被截（或後面還有行），只比前綴。
pub fn entry_matches(display: &str, target: &str) -> bool {
    let want = squash(first_line(target));
    if want.is_empty() {
        return false;
    }
    match display.trim().strip_suffix('…') {
        Some(head) => {
            let head = squash(head);
            !head.is_empty() && want.starts_with(&head)
        }
        None => squash(display) == want,
    }
}

/// 確認頁印的字是不是目標。整則對上最好；只對上前綴時，要確實是被截斷的長度（見 [`TRUNCATED_MIN_LINES`]）。
pub fn confirm_matches(quoted: &[String], target: &str) -> bool {
    let shown = squash(&quoted.join("\n"));
    let want = squash(target);
    if shown.is_empty() {
        return false;
    }
    shown == want || (want.starts_with(&shown) && quoted.len() >= TRUNCATED_MIN_LINES)
}

/// 目標在選單上要跳過幾則「第一行一樣」的較新訊息（由新到舊找，第 `skip+1` 個對上的才是）。
pub fn same_first_line(a: &str, b: &str) -> bool {
    squash(first_line(a)) == squash(first_line(b))
}

// ───────────── pane（可注入） ─────────────

pub trait Pane: Send + Sync {
    fn read(&self) -> BoxFuture<'_, anyhow::Result<String>>;
    fn send_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
    fn send_keys<'a>(&'a self, keys: &'a [&'a str]) -> BoxFuture<'a, anyhow::Result<()>>;
}

pub struct HerdrPane {
    pub client: crate::herdr::HerdrClient,
    pub pane_id: String,
}

impl Pane for HerdrPane {
    /// 帶樣式讀（`format: ansi`）：跟其他看輸入列的地方同一支（7178806b）。純文字分不出 claude 2.1.280 輸入列裡 dim 的
    /// 「建議下一句」和使用者打的字，會把建議句當成有字、擋成 `composer_busy`。
    fn read(&self) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move { lifecycle::read_styled(&self.client, &self.pane_id, "visible", 80).await })
    }
    fn send_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.client.pane_send_text(&self.pane_id, text).await })
    }
    fn send_keys<'a>(&'a self, keys: &'a [&'a str]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.client.pane_send_keys(&self.pane_id, keys).await })
    }
}

// ───────────── 狀態機 ─────────────

/// 為什麼沒倒成。`reason()` 是 409 的機器可讀理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// 選單或確認頁本來就開著（有人在終端裡操作）。什麼都沒按。
    UiBusy,
    /// 輸入列裡已經有字。什麼都沒打。
    ComposerBusy,
    /// 打了 `/rewind` 選單沒出來。
    MenuNotShown,
    /// 一路往上到頂都沒找到那一則。
    NotInMenu,
    /// 選了那一則，確認頁沒出來。
    ConfirmNotShown,
    /// 確認頁印的不是那一則（帶畫面上的字）。已 Esc。
    TextMismatch(String),
    /// 按了 Restore 之後畫面沒有離開 rewind：不知道到底倒了沒有。
    Unconfirmed,
    /// 讀不到／送不出 herdr。
    Pane(String),
}

impl Fail {
    pub fn reason(&self) -> &'static str {
        match self {
            Fail::UiBusy => "rewind_ui_open",
            Fail::ComposerBusy => "composer_busy",
            Fail::MenuNotShown => "menu_not_shown",
            Fail::NotInMenu => "not_in_menu",
            Fail::ConfirmNotShown => "confirm_not_shown",
            Fail::TextMismatch(_) => "text_mismatch",
            Fail::Unconfirmed => "rewind_unconfirmed",
            Fail::Pane(_) => "pane_unavailable",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Fail::UiBusy => "終端裡的倒回選單已經開著（有人正在操作），沒有動它。".into(),
            Fail::ComposerBusy => "終端的輸入列裡已經有字，沒有動它；清掉再倒回。".into(),
            Fail::MenuNotShown => "打了 /rewind 選單沒有出來（這版 claude 可能不支援），沒有倒回。".into(),
            Fail::NotInMenu => "倒回選單裡找不到這一則（可能在更早的 session，或送出時沒有落地），沒有倒回。".into(),
            Fail::ConfirmNotShown => "選了那一則，確認頁沒有出來，已取消。".into(),
            Fail::TextMismatch(shown) => format!("確認頁上的訊息跟要倒回的那則對不上，已取消、沒有倒回。畫面上是：「{}」", preview(shown, 80)),
            Fail::Unconfirmed => "按下 Restore 之後畫面沒有離開倒回選單，不確定有沒有倒回；請到終端看一下。".into(),
            Fail::Pane(e) => format!("讀不到或打不進終端：{e}"),
        }
    }
}

/// 倒成了。`pane_cleared`＝CLI 放回輸入列的原文已經清掉（清不掉只記 log，倒回本身已經成立）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub pane_cleared: bool,
}

/// 讀到的畫面（可能帶樣式）→ 下面所有判讀用的純文字：去掉樣式，輸入列裡只有 dim 建議句時把它抹掉
/// （`delivery::plain_without_hints`，跟送達、補 Enter、清框同一套）。有真的字就原樣留著。
pub fn screen_text(raw: &str) -> String {
    lifecycle::plain_without_hints("claude", raw)
}

async fn read(pane: &dyn Pane) -> Result<String, Fail> {
    pane.read().await.map(|raw| screen_text(&raw)).map_err(|e| Fail::Pane(e.to_string()))
}

async fn keys(pane: &dyn Pane, k: &[&str]) -> Result<(), Fail> {
    pane.send_keys(k).await.map_err(|e| Fail::Pane(e.to_string()))
}

/// 在 `ms` 內輪詢畫面直到 `f` 給出答案。
async fn wait_for<T>(pane: &dyn Pane, ms: u64, mut f: impl FnMut(&str) -> Option<T>) -> Result<Option<T>, Fail> {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let s = read(pane).await?;
        if let Some(v) = f(&s) {
            return Ok(Some(v));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

/// 退出 rewind：Esc 到畫面離開選單／確認頁為止（確認頁要兩下）。
async fn back_out(pane: &dyn Pane) {
    for _ in 0..3 {
        match read(pane).await {
            Ok(s) if !in_rewind_ui(&s) => return,
            Err(_) => return,
            _ => {}
        }
        let _ = pane.send_keys(&["Escape"]).await;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS.max(POLL_MS))).await;
    }
    tracing::warn!("rewind: the rewind UI would not close after three Esc");
}

/// 驅動 `/rewind` 倒回到 `target` 之前。`skip`：從新到舊，第一行一樣的較新訊息有幾則要跳過。
/// 呼叫端持 bot 鎖、已確認閒著。
pub async fn drive(pane: &dyn Pane, target: &str, skip: usize) -> Result<Done, Fail> {
    let s = read(pane).await?;
    if in_rewind_ui(&s) {
        return Err(Fail::UiBusy);
    }
    if lifecycle::composer_text("claude", &s).is_some() {
        return Err(Fail::ComposerBusy);
    }
    pane.send_text("/rewind").await.map_err(|e| Fail::Pane(e.to_string()))?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    keys(pane, &["Enter"]).await?;
    let Some(mut menu) = wait_for(pane, MENU_WAIT_MS, parse_menu).await? else {
        // 選單沒認出來：畫面上有 rewind 的東西就 Esc 關掉；沒有的話 `/rewind` 可能還躺在輸入列，清掉
        // （輸入列原本是空的，清的只會是我們打的字）。不在 rewind 畫面上按 ctrl+c：那裡的 `❯` 列不是輸入列。
        let s = read(pane).await?;
        if in_rewind_ui(&s) {
            back_out(pane).await;
        } else if lifecycle::composer_text("claude", &s).is_some() {
            let _ = keys(pane, &["ctrl+c"]).await;
        }
        return Err(Fail::MenuNotShown);
    };

    let mut remaining = skip;
    let mut found = false;
    for _ in 0..MAX_STEPS {
        keys(pane, &["Up"]).await?;
        let prev = menu.block.clone();
        // 游標沒動＝已經在最上面。
        let Some(next) = wait_for(pane, STEP_WAIT_MS, |s| parse_menu(s).filter(|m| m.block != prev)).await? else { break };
        menu = next;
        if menu.selected.as_deref().is_some_and(|d| entry_matches(d, target)) {
            if remaining == 0 {
                found = true;
                break;
            }
            remaining -= 1;
        }
    }
    if !found {
        back_out(pane).await;
        return Err(Fail::NotInMenu);
    }

    keys(pane, &["Enter"]).await?;
    let Some(_) = wait_for(pane, CONFIRM_WAIT_MS, parse_confirm).await? else {
        back_out(pane).await;
        return Err(Fail::ConfirmNotShown);
    };
    #[cfg(test)]
    lifecycle::race_point::hit("rewind_before_restore", target).await;
    let latest = read(pane).await?;
    let Some(confirm) = parse_confirm(&latest) else {
        back_out(pane).await;
        return Err(Fail::ConfirmNotShown);
    };
    if !confirm_matches(&confirm.quoted, target) {
        back_out(pane).await;
        return Err(Fail::TextMismatch(confirm.quoted.join("\n")));
    }
    // 對過字才選。按 `1` 而不是 Enter：選項是編號的，`1` 直接選 Restore，不靠游標位置，也不用看得到選項（矮 pane 會被擠掉）。
    keys(pane, &["1"]).await?;
    let Some(refill) = wait_for(pane, RESTORE_WAIT_MS, |s| (!in_rewind_ui(s)).then(|| lifecycle::composer_text("claude", s))).await? else {
        return Err(Fail::Unconfirmed);
    };
    // 原文交給網頁；pane 的輸入列要空著，下一則才不會接在後面。只清 CLI 放回來的那段：輸入列看得到的是目標的連續一段
    // （長的只看得到尾巴幾行，14 列實測）才清。
    let pane_cleared = match refill {
        None => true,
        Some(text) if !squash(&text).is_empty() && squash(target).contains(&squash(&text)) => {
            keys(pane, &["ctrl+c"]).await?;
            let cleared = wait_for(pane, CLEAR_WAIT_MS, |s| lifecycle::composer_text("claude", s).is_none().then_some(())).await?.is_some();
            // 清掉之後 claude 會顯示幾秒「Press Ctrl-C again to exit」：這段時間再來一個 ctrl+c（停機、中斷）就把它關掉了。
            // 握著 bot 鎖等提示消失才放手（2026-09-23 實機：約 3 秒）。
            let _ = wait_for(pane, HINT_WAIT_MS, |s| (!s.contains(CTRL_C_HINT)).then_some(())).await?;
            cleared
        }
        Some(other) => {
            tracing::warn!(composer = %preview(&other, 60), "rewind: the composer holds something other than the rewound prompt; leaving it");
            false
        }
    };
    Ok(Done { pane_cleared })
}

// ───────────── 端點 ─────────────

#[derive(Debug, Deserialize)]
pub struct RewindIn {
    pub message_id: String,
}

fn conflict(reason: &str, message: &str) -> LcError {
    LcError::conflict(reason, json!({"message": message}))
}

fn preview(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

/// `POST /api/bots/{id}/rewind`
pub async fn post_rewind(State(app): State<Arc<App>>, Path(bot_id): Path<String>, Json(b): Json<RewindIn>) -> LcResult<Json<Value>> {
    rewind(&app, &bot_id, &b.message_id, None).await.map(Json)
}

/// `pane`：測試注入的假 pane；`None`＝這個 run 的真 pane。
pub async fn rewind(app: &Arc<App>, bot_id: &str, message_id: &str, pane: Option<Arc<dyn Pane>>) -> LcResult<Value> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.kind != "claude" {
        return Err(conflict("unsupported_kind", "只有 claude 能倒回：codex／grok 沒有對應的 /rewind。"));
    }
    // 使用者自己 default session 的 pane 只觀察、不代打（SPEC §6.5.1）。
    lifecycle::refuse_default_session(&bot)?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    let msg: db::Message = sqlx::query_as("SELECT * FROM messages WHERE id = ? AND conversation_id = ?")
        .bind(message_id)
        .bind(&conv)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("message".into()))?;
    if msg.role != "user" {
        return Err(conflict("not_a_user_message", "只能倒回到一則使用者訊息。"));
    }
    if msg.rewound_at.is_some() {
        return Err(conflict("already_rewound", "這一則已經倒回掉了。"));
    }
    // 送出的字：排隊的網頁 prompt 記在 turn 上（跟畫面上的原文可能不一樣）。
    let prompt_text: Option<String> = match msg.turn_id.as_deref() {
        Some(t) => sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?").bind(t).fetch_optional(&app.db).await.map_err(up)?.flatten(),
        None => None,
    };
    let target = prompt_text.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| msg.content.clone());
    // 同一行開頭的較新訊息（沒被倒掉的）要在選單上跳過幾則。
    let later: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM messages WHERE conversation_id = ? AND role = 'user' AND rewound_at IS NULL
           AND rowid > (SELECT rowid FROM messages WHERE id = ?)",
    )
    .bind(&conv)
    .bind(&msg.id)
    .fetch_all(&app.db)
    .await
    .map_err(up)?;
    let skip = later.iter().filter(|c| same_first_line(c, &target)).count();

    // 持 bot 鎖到打完字：`prompt` 拿同一把，期間不會有新的 prompt 打進這個 pane。
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| conflict("not_running", "bot 沒在跑。"))?;
    if let Some(why) = busy_reason(app, bot_id, &run).await? {
        return Err(LcError::conflict("not_idle", json!({"busy": why, "message": "它正在忙，等這一回合結束再倒回。"})));
    }
    let pane: Arc<dyn Pane> = match pane {
        Some(p) => p,
        None => {
            let pane_id = run.pane_id.clone().filter(|p| !p.trim().is_empty()).ok_or_else(|| conflict("no_pane", "這個 run 沒有 pane。"))?;
            let client = app.herdr_for_run(&run).await.ok_or_else(|| up(format!("no Herdr session is available for run `{}`", run.id)))?;
            Arc::new(HerdrPane { client, pane_id })
        }
    };
    // 直接對 pane 打過字：之後的 prompt 改走打字路線（`slash::mark_pane_typed` 的理由）。記不下來就不打。
    lifecycle::mark_pane_typed(app, &run.id).await.map_err(up)?;
    let done = match drive(pane.as_ref(), &target, skip).await {
        Ok(d) => d,
        Err(f) => {
            tracing::warn!(bot = %bot.name, reason = f.reason(), "rewind did not happen");
            return Err(match f {
                Fail::Pane(_) => up(f.message()),
                _ => LcError::conflict(f.reason(), json!({"message": f.message()})),
            });
        }
    };

    let now = db::now();
    let hidden = mark_rewound(app, &conv, &msg.id, &now).await.map_err(|e| {
        LcError::uncommitted("rewind_marks_uncommitted", &run.id, "已經倒回了，但對話紀錄沒標記成功；重新整理後被倒掉的訊息可能還顯示著", e)
    })?;
    let note = format!("已倒回到這則之前：「{}」。之後的 {hidden} 則不在對話脈絡裡了（紀錄保留）。", preview(&msg.content, 40));
    let _ = lifecycle::insert_message(app, &conv, None, "system", &note, "system", false, None).await;
    app.emit("messages_rewound", json!({"bot_id": bot_id, "message_id": msg.id, "rewound_at": now})).await;
    tracing::info!(bot = %bot.name, hidden, pane_cleared = done.pane_cleared, "rewound the conversation");
    Ok(json!({
        "rewound": true,
        "message_id": msg.id,
        "text": msg.content,
        "hidden": hidden,
        "pane_cleared": done.pane_cleared,
    }))
}

/// 這一則與之後的都標成倒回（標記不刪）。回標了幾則。
async fn mark_rewound(app: &Arc<App>, conv: &str, message_id: &str, now: &str) -> anyhow::Result<u64> {
    Ok(sqlx::query(
        "UPDATE messages SET rewound_at = ?
          WHERE conversation_id = ? AND rewound_at IS NULL
            AND rowid >= (SELECT rowid FROM messages WHERE id = ?)",
    )
    .bind(now)
    .bind(conv)
    .bind(message_id)
    .execute(&app.db)
    .await?
    .rows_affected())
}

/// 鎖裡查：要閒著。排著的也算忙：鎖一放就會送進倒回後的對話，那不一定是使用者要的。
async fn busy_reason(app: &Arc<App>, bot_id: &str, run: &db::Run) -> LcResult<Option<&'static str>> {
    Ok(if run.state != "running" {
        Some("not_running")
    } else if run.agent_status != "idle" {
        Some(match run.agent_status.as_str() {
            "working" => "working",
            "blocked" => "blocked",
            _ => "unknown_status",
        })
    } else if db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some() {
        Some("turn_in_flight")
    } else if db::queued_turn_for_bot(&app.db, bot_id).await.map_err(up)?.is_some() {
        Some("queued_turn")
    } else {
        None
    })
}

#[cfg(test)]
mod tests;
