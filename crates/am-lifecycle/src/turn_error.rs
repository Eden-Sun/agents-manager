//! 「這一回合其實被 API 斷線截斷了」的偵測（見 SPEC §4.3a）。
//! 斷線時 hook 照樣送 Stop、herdr 照樣報 idle，回合會被誤記成 `completed`，所以要讀 pane 補認。

use crate::db;
use crate::lifecycle;
use crate::lifecycle::s6_ports::{TurnErrorContext, TurnErrorQuotaAccess};
use am_core::{LimitHit as PortLimitHit, QuotaSnapshot, Window as PortWindow};
use am_ports::DbContext;
use anyhow::Result;
/// 錯誤行後面只會剩下狀態列與輸入框那幾行 chrome。
const TAIL_LINES: usize = 30;

/// 額度拒絕也算：claude 0 秒就 `done`，對使用者一樣是「送了沒回」（2026-09-10）。
fn is_api_error(body: &str) -> bool {
    lower_is_api_error(&body.to_ascii_lowercase())
}

fn lower_is_api_error(lower: &str) -> bool {
    let banner = lower.starts_with("api error:")
        || (lower.starts_with("api error") && (lower.contains("retrying") || lower.contains("attempt ") || lower.contains("connection")));
    banner || is_quota_limit_lower(lower)
}

/// 跟斷線分開認：重送救不回來，要等重置或換模型（2026-09-12 使用者：「已用盡卻沒有正確的提示」）。
pub fn is_quota_limit(body: &str) -> bool {
    is_quota_limit_lower(&body.to_ascii_lowercase())
}

/// `StopFailure` 帶的原因是不是「這個帳號的額度用完了」（issue #108）。比 `hookrecv::classify_failure` 的
/// rate limit 窄：那一類還包含 `overloaded`／一般 429 這種一下就好的限流，記成撞限會把排著的派工與之後的
/// 派送壓上好幾個小時。認的是跟畫面同一套的橫幅（前面可能多了 `API Error:` 之類的字）與 `usage limit`；
/// 月度花費上限那類照 [`is_quota_limit`] 的規則不算。
pub fn is_quota_exhaustion(detail: &str) -> bool {
    detail.lines().any(exhaustion_line)
}

/// 一行是不是額度用完的橫幅：前導記號與 `API Error:`／`ERROR:` 剝掉後，整行就是橫幅的形狀。
/// 句中提到 `usage limit`（一般回覆、hook 的長說明）不算；超過 [`BANNER_MAX_CHARS`] 的也不是橫幅。
fn exhaustion_line(line: &str) -> bool {
    let s = banner_start(line);
    let nested = after_resource_exhausted(s).map(banner_start);
    [Some(s), nested].into_iter().flatten().any(banner_shape)
}

/// 一行（已剝掉前導記號）是不是橫幅形狀。機器代碼（整行就是代碼，或整行是帶代碼欄位的 JSON 物件，
/// `{"type":"usage_limit_reached"…}`）不是句子，不受長度限制；其餘要求不超過 [`BANNER_MAX_CHARS`]。
fn banner_shape(s: &str) -> bool {
    let s = s.to_ascii_lowercase().replace('’', "'");
    let code = |w: &str| matches!(w, "usage_limit" | "usage_limit_reached" | "usage_limit_exceeded");
    if code(s.trim_end_matches('.')) || (s.starts_with('{') && s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(code)) {
        return true;
    }
    if s.chars().count() > BANNER_MAX_CHARS {
        return false;
    }
    agy_line(&s) || is_quota_limit_lower(&s) || usage_limit_banner(&s)
}

/// agy 的 gRPC 錯誤把橫幅包在 `RESOURCE_EXHAUSTED (code N): ` 後面，取它之後的部分。
fn after_resource_exhausted(s: &str) -> Option<&str> {
    let lower = s.to_ascii_lowercase();
    let at = lower.find("resource_exhausted (code ")?;
    let rest = &s[at..];
    let colon = rest.find("): ")?;
    Some(&rest[colon + "): ".len()..])
}

const BANNER_MAX_CHARS: usize = 200;

/// `usage limit` 桶的橫幅（`You've hit your usage limit. …`、`Claude usage limit reached. …`、`Usage limit reached · …`）：`limit` 之後要接橫幅收尾。
fn usage_limit_banner(lower: &str) -> bool {
    if let Some(rest) = lower.strip_prefix("claude usage limit reached").or_else(|| lower.strip_prefix("usage limit reached")) {
        return banner_tail(rest.strip_prefix(" again").unwrap_or(rest));
    }
    lower.strip_prefix("you've hit your usage limit").or_else(|| lower.strip_prefix("you've reached your usage limit")).is_some_and(banner_tail)
}

/// 去掉行首的記號（`⏺`、`■`、`•`、`⎿`、`⚠`、提示符）與 `API Error:`／`ERROR:` 前綴，回傳橫幅本身。
fn banner_start(line: &str) -> &str {
    let s = line.trim_start_matches(|c: char| c.is_whitespace() || "⏺■•⎿⚠│┃>›❯".contains(c));
    for prefix in ["api error:", "error:"] {
        if s.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix)) {
            return s[prefix.len()..].trim_start();
        }
    }
    s
}

/// agy 額度列的判定（輸入須已轉小寫）：只剝前導的 ⚠ 與空白，不剝 `■`／`❯` 之類——那是助理回覆與使用者
/// 輸入的記號，橫幅前面不會有。
fn agy_line(lower: &str) -> bool {
    let s = strip_warning(lower);
    (s.starts_with("individual quota reached") && s.contains("resets in")) || agy_prepaid_line(s)
}

/// agy 1.5.17：企業帳號預付餘額為空，CLI 直接停、不再重試；照額度用盡處理（沒有重置時間，到期交保底）。
/// 舊版的 `You have exhausted your quota on this model.` 整行也認（同一種耗盡）。
fn agy_prepaid_line(s: &str) -> bool {
    let prepaid = s.strip_prefix("your organization's prepaid balance is insufficient").is_some_and(|tail| tail.is_empty() || tail.starts_with('.'));
    prepaid || s.trim_end() == "you have exhausted your quota on this model."
}

/// 橫幅前導的 ⚠ 與空白。
fn strip_warning(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || c == '⚠')
}

/// 第 `i` 行是不是 agy 額度橫幅的結尾（[`api_error_line`] 由下往上呼叫）。兩種情況：本行自己就是完整的
/// `Individual quota reached … Resets in …`；或本行是上一行 `… Resets in` 折下來的重置時間（`13m11s.`）。
/// 只接這兩種相鄰行，不把整個畫面接起來比。
fn agy_banner_at(lines: &[&str], i: usize) -> Option<String> {
    let text = strip_warning(undecorated(lines[i]));
    if agy_line(&text.to_ascii_lowercase()) {
        return Some(text.to_string());
    }
    let head = strip_warning(undecorated(lines[i.checked_sub(1)?]));
    let head_lower = head.to_ascii_lowercase();
    let waiting = agy_line(&head_lower) && head_lower.trim_end().ends_with("resets in");
    let duration_only = text.trim_end_matches('.');
    let pure = !duration_only.is_empty() && duration_only.chars().all(|c| c.is_ascii_alphanumeric() || c.is_whitespace());
    (waiting && pure && reset_seconds(duration_only).is_some()).then(|| format!("{head} {text}"))
}

fn is_quota_limit_lower(lower: &str) -> bool {
    let reached = lower.starts_with("you've reached your") && banner_sentence(lower);
    let agy = agy_line(lower);
    let hit = (lower.starts_with("you've hit your") || lower.starts_with("you hit your")) && banner_sentence(lower) && limit_bucket(lower) != LimitBucket::Unknown;
    reached || hit || agy
}

/// `limit` 之後接的是橫幅的收尾（`· resets …`、`. Try again …`、`… `、行尾）才算橫幅句。一般回覆
/// `You've hit your weekly goal…`、`You've reached your goal; the limit was raised…` 的 `limit` 後面是別的句子，不算。
fn banner_sentence(lower: &str) -> bool {
    let Some(at) = lower.find("limit") else { return false };
    banner_tail(&lower[at + "limit".len()..])
}

fn banner_tail(after_limit: &str) -> bool {
    let tail = after_limit.trim_start();
    tail.is_empty() || ["·", ".", "…", "(", "resets", "try again", "upgrade"].iter().any(|p| tail.starts_with(p))
}

/// 橫幅說的是哪一桶。CLI 2.1.273 的字串表就是這幾種 rate limit：
/// `five_hour`→`session limit`、`seven_day`→`weekly limit`、`seven_day_opus`→`Opus limit`、
/// `seven_day_sonnet`→`Sonnet limit`、`seven_day_overage_included`→`Fable limit`。
/// Opus／Sonnet 跟 Fable 一樣是**模型自己的**週桶，不是整個身份的 7d（review3 c4 M1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitBucket {
    Session,
    Weekly,
    Fable,
    /// 模型專屬的週桶（`Opus limit`／`Sonnet limit`）：只擋跑那個模型的 bot。
    Model(&'static str),
    Unknown,
}

fn limit_bucket(lower: &str) -> LimitBucket {
    if lower.contains("fable") {
        LimitBucket::Fable
    } else if lower.contains("opus limit") {
        LimitBucket::Model("opus")
    } else if lower.contains("sonnet limit") {
        LimitBucket::Model("sonnet")
    } else if lower.contains("weekly") {
        LimitBucket::Weekly
    } else if lower.contains("session limit") || lower.contains("5-hour") || lower.contains("five-hour") || lower.starts_with("individual quota reached") {
        LimitBucket::Session
    } else {
        LimitBucket::Unknown
    }
}

/// 那一桶沒有讀數時，撞限要撐多久才自己過期。claude 這一側沒有 `clear_limit_hit`（Fable 用完換
/// opus 照樣能跑，成功回合不能當作解除），所以 `until=None` ＝ 永遠不過期：交辦會卡在 `quota_blocked`
/// 直到有人重啟 daemon（review 2026-09-16）。寧可保守地等一個視窗長度，也不要沒有出口。
/// 給下游用的桶名，跟 `Quota` 的欄位同名。認不出來就 `None`——不要編一個。
fn bucket_name(lower: &str) -> Option<String> {
    match limit_bucket(lower) {
        LimitBucket::Session => Some("five_hour".into()),
        LimitBucket::Weekly => Some("seven_day".into()),
        LimitBucket::Fable => Some("fable".into()),
        LimitBucket::Model(m) => Some(m.to_string()),
        LimitBucket::Unknown => None,
    }
}

/// 一段文字裡的 claude 撞限橫幅說的是哪一桶（[`bucket_name`] 的對外版本）。開機回填用它從 parked 交辦記下的
/// `error`（`帳號撞到用量上限（…）：<橫幅>`）把桶名找回來，`mission::pick` 才不必把回填的格子一律猜成週窗。
pub fn banner_bucket(text: &str) -> Option<String> {
    bucket_name(&text.to_ascii_lowercase())
}

fn fallback_until(lower: &str, at: &str) -> Option<String> {
    let hours = match limit_bucket(lower) {
        LimitBucket::Session => 5,
        LimitBucket::Weekly | LimitBucket::Fable | LimitBucket::Model(_) => 24 * 7,
        LimitBucket::Unknown => 5,
    };
    let base = chrono::DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&chrono::Utc);
    Some(crate::db::iso_at(base + chrono::Duration::hours(hours)))
}

/// agy 的撞限列提供相對重置時間（例如 `Resets in 3h 12m`）；可讀時採用，否則由 5h 窗長保底。
fn agy_reset_until(line: &str, at: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let seconds = reset_seconds(lower.split_once("resets in")?.1)?;
    let base = chrono::DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&chrono::Utc);
    Some(crate::db::iso_at(base + chrono::Duration::seconds(seconds)))
}

/// `Resets in` 後面的時長：`3h 12m`、`13m11s`、`45s`、`2h`（可以相連或隔空白，尾端的句號不管）。
/// 任何一段數字沒有單位、單位認不出、或總和不是正數都回 `None`——寧可不設到期，也不要猜成 0 或現在。
fn reset_seconds(tail: &str) -> Option<i64> {
    let mut chars = tail.trim_start().chars().peekable();
    let mut total = 0i64;
    loop {
        let mut digits = String::new();
        while let Some(c) = chars.next_if(|c| c.is_ascii_digit()) {
            digits.push(c);
        }
        let value: i64 = digits.parse().ok()?;
        let mut unit = String::new();
        while let Some(c) = chars.next_if(|c| c.is_ascii_alphabetic()) {
            unit.push(c.to_ascii_lowercase());
        }
        let each = match unit.as_str() {
            "d" | "day" | "days" => 86_400,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            _ => return None,
        };
        total = total.checked_add(value.checked_mul(each)?)?;
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            break;
        }
    }
    (total > 0).then_some(total)
}

/// 把橫幅說的那一桶標成 100%，回傳它的重置時間（撞限到那時才解除）。認不出是哪一桶時照舊先 5h 再 7d。
///
/// Opus／Sonnet 的週桶 daemon 沒有量表可放：**一格都不標**（標 7d 會讓同身份所有 bot 看起來週額度用光），
/// 重置時間借 7d 那格——`/usage` 的 `weekly_scoped` 列跟 `weekly_all` 同一個重置週期（review3 c4 M1）。
///
/// 重置時間不晚於撞的那一刻（`at`）的讀數是**上一個窗**的：閒置的 5h 窗，`/usage` 照樣回上一個重置時間
/// （2026-09-19 `claude:cc1`：0%、−0.2h）。它說不出這次撞限什麼時候解除——拿來當到期，撞限一記下就過期、被
/// `quota::set` 丟掉，派工照送（#236）。所以不拿它的時間：認不出桶名時改看下一個窗；明講的那一桶照樣標滿，
/// 過期的重置時間丟掉，到期交給保底（[`fallback_until`]）。
fn saturate_bucket(q: &mut QuotaSnapshot, lower: &str, at: &str) -> Option<String> {
    let hit_at = chrono::DateTime::parse_from_rfc3339(at).ok();
    let stale = |w: &PortWindow| {
        let reset = w.resets_at.as_deref().and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok());
        matches!((reset, hit_at), (Some(r), Some(a)) if r <= a)
    };
    let current = |w: &Option<PortWindow>| w.as_ref().is_some_and(|w| !stale(w));
    let full = |w: &mut Option<PortWindow>| {
        w.as_mut().map(|w| {
            w.used_pct = 100.0;
            if stale(w) {
                w.resets_at = None;
            }
            w.resets_at.clone()
        })
    };
    match limit_bucket(lower) {
        LimitBucket::Fable => full(&mut q.fable).flatten(),
        LimitBucket::Model(_) => q.seven_day.as_ref().filter(|w| !stale(w)).and_then(|w| w.resets_at.clone()),
        LimitBucket::Weekly => full(&mut q.seven_day).flatten(),
        LimitBucket::Session => full(&mut q.five_hour).flatten(),
        LimitBucket::Unknown if !current(&q.five_hour) && current(&q.seven_day) => full(&mut q.seven_day).flatten(),
        LimitBucket::Unknown => match full(&mut q.five_hour) {
            Some(until) => until,
            None => full(&mut q.seven_day).flatten(),
        },
    }
}

/// 錯誤行之後出現非 chrome 行，代表錯誤已被重試蓋過。
fn is_chrome(s: &str) -> bool {
    s.is_empty()
        || s == "❯"
        || s == "›"
        || lifecycle::screen::is_codex_idle_prompt(s)
        || lifecycle::is_noise(s)
        || lifecycle::is_activity_shape(s)
        || s.contains("Update installed")
        || is_token_count(s)
        || is_agy_chrome(s)
}

fn is_agy_chrome(s: &str) -> bool {
    s == ">" || s.starts_with("Error ID:") || s.contains("? for shortcuts")
}

fn is_token_count(s: &str) -> bool {
    let Some(head) = s.strip_suffix(" tokens").or_else(|| s.strip_suffix(" token")) else { return false };
    !head.is_empty() && head.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.' || c == 'k' || c == 'K')
}

/// 只剝框線、保留前導記號——`is_noise` 要靠它認 spinner。
fn undecorated(line: &str) -> &str {
    line.trim().trim_matches(|c| "│┃".contains(c)).trim()
}

fn body_of(s: &str) -> &str {
    match s.chars().next() {
        Some(c) if !c.is_alphanumeric() && c != '❯' && c != '›' => s[c.len_utf8()..].trim_start(),
        _ => s,
    }
}

/// codex 模型滿載的橫幅（#1673）：原文是 `■ Selected model is at capacity. Please try a different model.`，
/// 釘上的是人話，原文附在後面。不是額度撞限，所以不走 `limit_hit` 那套。
const MODEL_AT_CAPACITY: &str = "模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）";

/// 這一行 `runs.turn_error` 是哪一種（#1674，`run.turn_error_kind`）：網頁靠它分辨，不去比對文字。
/// `model_at_capacity`＝OpenAI 模型滿載（#1673，不是額度、也不是卡住）；`quota_limit`＝額度撞限；其餘 `api_error`。
pub fn kind_of(line: &str) -> &'static str {
    if line == MODEL_AT_CAPACITY {
        "model_at_capacity"
    } else if is_quota_limit(line) {
        "quota_limit"
    } else {
        "api_error"
    }
}

/// codex 把 API 的 4xx／5xx 回應原樣印成 `■ {"type":"error","status":400,"error":{"type":"invalid_request_error","message":"…"}}`
/// （#1675：子 agent 連上舊版共用 app-server，`gpt-6.1-sol` 被 400 拒絕，回合卻照常收成完成）。
/// 只認行首的 `■` 加上帶 `error.message` 的 JSON；釘上的是狀態碼、錯誤種類與訊息。
fn codex_json_error(line: &str) -> Option<String> {
    if !line.starts_with('■') {
        return None;
    }
    let body = undecorated(line).strip_prefix('■')?.trim();
    if !body.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    let message = err.get("message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    let status = v.get("status").and_then(serde_json::Value::as_u64).map(|s| s.to_string());
    let kind = err.get("type").and_then(serde_json::Value::as_str);
    let head = [status.as_deref(), kind].into_iter().flatten().collect::<Vec<_>>().join(" ");
    Some(if head.is_empty() { format!("API 拒絕這次請求：{message}") } else { format!("API 拒絕這次請求（{head}）：{message}") })
}

/// 只認**行首**的 `■`：codex 的回覆行是 `•`，縮排的續行是引文，談到這句話的回覆不是橫幅。
fn is_model_at_capacity(line: &str) -> bool {
    line.starts_with('■') && body_of(undecorated(line)).to_ascii_lowercase().starts_with("selected model is at capacity")
}

/// codex 的資安過濾（2026-10-17 w0:pDX）：OpenAI 把整段回覆擋掉，畫面只剩 `ⓘ This content can’t be shown`
/// 加幾行縮排說明（Daybreak 申請連結），rollout 的 `task_complete` 只有 `error`、沒有回覆。hook 不來、備援也撈不到字，
/// 回合被靜靜收掉，使用者看起來就是卡住。釘上的是人話，原文附在後面。
const CONTENT_BLOCKED: &str = "OpenAI 資安過濾擋掉了這次回覆，請改寫請求、避開觸發的內容後重送（This content can’t be shown）";

/// 跟滿載橫幅同一條規則：`ⓘ` 要在原始行的行首，縮排的續行與 `•` 回覆是引文。
fn is_content_blocked(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('ⓘ') else { return false };
    let body = rest.trim().to_lowercase().replace('’', "'");
    body.starts_with("this content can't be shown")
}

/// 橫幅底下那幾行縮排說明：往上走過連續的縮排行，碰到的第一行是橫幅才算。說明最多四行，多看幾行留餘地。
fn is_content_blocked_detail(lines: &[&str], at: usize) -> bool {
    let indented = |l: &str| l.starts_with(' ') && !l.trim().is_empty();
    if !indented(lines[at]) {
        return false;
    }
    let mut i = at;
    for _ in 0..8 {
        let Some(prev) = i.checked_sub(1) else { return false };
        if is_content_blocked(lines[prev]) {
            return true;
        }
        if !indented(lines[prev]) {
            return false;
        }
        i = prev;
    }
    false
}

/// 必須是「最後一件事」：重試成功後橫幅仍留在畫面上，但下面還有回覆，不算斷線。
pub fn api_error_line(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    let survey = crate::agy_screen::feedback_survey_span(screen);
    for (i, line) in lines.iter().enumerate().skip(start).rev() {
        if survey.is_some_and(|(top, bottom)| (top..=bottom).contains(&i)) {
            continue;
        }
        let raw = undecorated(line);
        let body = body_of(raw);
        if is_api_error(body) {
            return Some(body.to_string());
        }
        if is_model_at_capacity(line) {
            return Some(MODEL_AT_CAPACITY.to_string());
        }
        if let Some(reason) = codex_json_error(line) {
            return Some(reason);
        }
        if is_content_blocked(line) || is_content_blocked_detail(&lines, i) {
            return Some(CONTENT_BLOCKED.to_string());
        }
        if let Some(banner) = agy_banner_at(&lines, i) {
            return Some(banner);
        }
        if !is_chrome(raw) {
            return None;
        }
    }
    None
}

/// 呼叫端要持有 bot lock。回合已被 hook 收掉後也要跑——斷線回合正是 hook 照常送 Stop 的那種。
pub async fn capture(app: &impl TurnErrorContext, bot_id: &str, expected_run_id: &str) -> Result<bool> {
    let db = DbContext::new(app.db().clone());
    let Some(run) = db::active_run(db.pool(), bot_id).await? else { return Ok(false) };
    if run.id != expected_run_id {
        return Ok(false);
    }
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(false) };
    let Some(screen) = app.read_run_pane_recent_unwrapped(&run, pane_id, 200).await? else { return Ok(false) };
    let Some(line) = api_error_line(&screen) else { return Ok(false) };
    if run.turn_error.as_deref() == Some(line.as_str()) {
        return Ok(false);
    }
    let Some(bot) = db::bot(db.pool(), bot_id).await? else { return Ok(false) };
    let conversation_id = db::conversation_id(db.pool(), bot_id).await?;
    let turn = last_turn(&db, &run.id).await?;
    let partial = if bot.kind == "codex" {
        if let Some(t) = turn.as_ref() {
            match lifecycle::turn_echo_texts(app, &t.id).await {
                Ok(sent) => lifecycle::codex_partial_reply(&screen, &sent),
                Err(e) => {
                    tracing::debug!(turn = %t.id, error = ?e, "partial Codex reply could not be matched to its prompt");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let marked = if is_quota_limit(&line) {
        match bot.kind.as_str() {
            "claude" => mark_claude_limit_hit(app, &bot, &line).await,
            "agy" => mark_agy_limit_hit(app, &bot, &line).await,
            _ => Ok(()),
        }
    } else {
        Ok(())
    };

    let mut tx = db.pool().begin().await?;
    sqlx::query("UPDATE runs SET turn_error = ? WHERE id = ?").bind(&line).bind(&run.id).execute(&mut *tx).await?;
    let mut messages = Vec::new();
    if let (Some(t), Some(reply)) = (turn.as_ref(), partial.as_deref()) {
        let already_has_answer: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant')")
            .bind(&t.id)
            .fetch_one(&mut *tx)
            .await?;
        if !already_has_answer {
            messages.push(
                lifecycle::insert_message_tx(
                    &mut tx,
                    &conversation_id,
                    Some(&t.id),
                    "assistant",
                    reply,
                    "terminal_fallback",
                    true,
                    Some(&screen),
                )
                .await?,
            );
        }
    }
    messages.push(
        lifecycle::insert_message_tx(
            &mut tx,
            &conversation_id,
            turn.as_ref().map(|t| t.id.as_str()),
            "system",
            &line,
            "system",
            true,
            Some(&screen),
        )
        .await?,
    );
    let mut failed = None;
    if let Some(t) = turn.as_ref().filter(|t| t.status == "in_flight") {
        let res = crate::lifecycle::turn_controller::fail_on(
            &mut tx,
            &t.id,
            crate::lifecycle::turn_controller::DeliveryOnFail::Keep,
            "CLI 回報這一回合出錯",
        )
        .await?;
        if res == crate::lifecycle::turn_controller::Outcome::Applied {
            failed = Some(t.id.clone());
        }
    }
    tx.commit().await?;
    tracing::warn!(bot = %bot_id, run = %run.id, error = %line, "turn cut short by an API error");
    for message in messages {
        app.emit("message_added", serde_json::json!({ "bot_id": bot_id, "message": message })).await;
    }
    if let Some(t) = failed {
        app.turn_changed(&t).await?;
    }
    app.emit_bot_status(bot_id).await;
    marked.map(|()| true)
}

/// 新回合開始：上一回合的錯誤是舊的了。寫不進去回錯（#193），呼叫端（progress poller）之後再清：留著的話，這一回合
/// 斷在同一句錯誤上會被 [`capture`] 的「同一則只記一次」吞掉——回合不收、輸入框一直鎖著。
pub async fn clear(app: &(impl crate::capabilities::Db + crate::capabilities::BotStatusEmit), run_id: &str, bot_id: &str) -> Result<()> {
    let db = DbContext::new(app.db().clone());
    let res = sqlx::query("UPDATE runs SET turn_error = NULL WHERE id = ? AND turn_error IS NOT NULL")
        .bind(run_id)
        .execute(db.pool())
        .await;
    match res {
        Ok(r) => {
            if r.rows_affected() > 0 {
                app.emit_bot_status(bot_id).await;
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(run = %run_id, error = %e, "the previous turn's error could not be cleared; retrying while this turn runs");
            Err(e.into())
        }
    }
}

/// 不篩 status：斷線那回合可能已被 Stop hook 收成 `completed`。
async fn last_turn(db: &DbContext<sqlx::SqlitePool>, run_id: &str) -> Result<Option<db::Turn>> {
    Ok(sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE run_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1")
        .bind(run_id)
        .fetch_optional(db.pool())
        .await?)
}

#[cfg(any(test, feature = "test-hooks"))]
pub mod test_seams {
    use super::*;

    pub async fn last_turn(db: &DbContext<sqlx::SqlitePool>, run_id: &str) -> Result<Option<db::Turn>> {
        super::last_turn(db, run_id).await
    }
}

#[cfg(all(test, not(feature = "test-hooks")))]
mod tests {
    use super::{agy_reset_until, api_error_line, CONTENT_BLOCKED, is_quota_exhaustion, is_quota_limit, kind_of};

    /// pane w168:pE 的真實快照（2026-09-09）：回合被 API 斷線截斷，但收尾照樣寫 `done`。
    const CONNECTION_LOST: &str = "\
❯ 幫我把那段改掉
⏺ 我先看一下現在的實作。
  ⎿  Read daemon/src/lifecycle.rs

⏺ API Error: Connection lost mid-response. The response above may be incomplete.

✻ Baked for 5m 21s · done 12:56 AM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    #[test]
    fn connection_lost_is_the_last_thing_on_screen() {
        assert_eq!(
            api_error_line(CONNECTION_LOST).as_deref(),
            Some("API Error: Connection lost mid-response. The response above may be incomplete.")
        );
    }

    #[test]
    fn other_api_error_variants_count_too() {
        let screen = "❯ hi\n⏺ API Error: 500 Internal Server Error\n\n✻ Worked for 3s · done 1:07 AM\n❯\n";
        assert_eq!(api_error_line(screen).as_deref(), Some("API Error: 500 Internal Server Error"));
        let retry = "❯ hi\n✻ API error · Retrying in 0s · attempt 1/10\n❯\n";
        assert_eq!(api_error_line(retry).as_deref(), Some("API error · Retrying in 0s · attempt 1/10"));
    }

    #[test]
    fn a_retry_that_recovered_is_not_an_interruption() {
        let screen = "\
❯ hi
✻ API error · Retrying in 0s · attempt 1/10
⏺ 好了，改完了。
✻ Worked for 9s · done 1:07 AM
❯
";
        assert_eq!(api_error_line(screen), None);
    }

    #[test]
    fn usage_limit_refusal_counts_as_an_error() {
        let screen = "\
❯ 用戶回報 cf2go
  ⎿  2 skills available
  ⎿  You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.

✻ Crunched for 0s · done 4:11 PM
                  current: 2.1.266 · latest: 2.1.267 ✔ Update installed · Restart to update
─────────────────────────────────────────────────────────────────────────────────────────────
❯
";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.")
        );
    }

    #[test]
    fn codex_0157_rate_limit_suggestion_is_not_an_exhausted_quota() {
        let screen = include_str!("lifecycle/fixtures/codex-0.157-rate-limit-switch.txt");
        assert_eq!(api_error_line(screen), None);
        assert!(!is_quota_limit(screen));
        assert!(!is_quota_exhaustion(screen));
        assert!(is_quota_limit("You've hit your weekly limit. Switch to gpt-6-luna for lower credit usage."));
    }

    /// #1673：codex 模型滿載（`■ Selected model is at capacity…`，2026-10-13 截圖）是回合錯誤，原因寫人話；不是額度撞限。
    #[test]
    fn codex_model_at_capacity_is_a_turn_error_not_a_quota_limit() {
        let screen = "\
› 幫我看一下 repo 狀態
■ Selected model is at capacity. Please try a different model.

›
  gpt-6.1-sol medium · ~/project/agents-manager · 5h 82% left · weekly 97% left
";
        let line = api_error_line(screen).expect("滿載要命中");
        assert_eq!(line, "模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）");
        assert!(!is_quota_limit(&line));
        assert!(!is_quota_exhaustion(&line));
    }

    /// #1673：回覆談到這句話不算。codex 的回覆行是 `•`，縮排的續行也不是行首的 `■`。
    /// #1674：種類由 daemon 定，網頁不比對文字。
    #[test]
    fn turn_error_kind_tells_capacity_from_quota_and_plain_api_errors() {
        let capacity = api_error_line("› hi\n■ Selected model is at capacity. Please try a different model.\n\n›\n").unwrap();
        assert_eq!(kind_of(&capacity), "model_at_capacity");
        assert_eq!(kind_of("You've reached your 5-hour limit"), "quota_limit");
        assert_eq!(kind_of("API Error: Connection lost mid-response."), "api_error");
    }

    /// #1675：mac_builder 的 codex 子 agent 連上舊版共用 app-server，OpenAI 回 400 `invalid_request_error`，
    /// codex 把整段 JSON 原樣印成 `■` 橫幅。回合要記成錯誤，原因寫出狀態碼與訊息，不能收成「完成」。
    #[test]
    fn a_codex_json_error_banner_is_a_turn_error_with_the_reason() {
        let screen = "\
› 你是 `robins-hub-gd6afd` 的子 agent：先讀 console/AGENTS.md
■ {\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.\"}}

›
  gpt-6.1-sol medium · ~/project/sample-agents/projects/rt · 5h 90% left · weekly 98% left
";
        let line = api_error_line(screen).expect("400 的 JSON 橫幅要命中");
        assert_eq!(
            line,
            "API 拒絕這次請求（400 invalid_request_error）：The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."
        );
        assert_eq!(kind_of(&line), "api_error");
        assert!(!is_quota_limit(&line));
        assert!(!is_quota_exhaustion(&line));
    }

    /// 只認行首的 `■` 加上帶 `error.message` 的 JSON：回覆（`•`）、縮排引文、不是錯誤的 JSON 都不算。
    #[test]
    fn json_that_is_not_a_codex_error_banner_is_left_alone() {
        let json = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"nope"}}"#;
        assert_eq!(api_error_line(&format!("› hi\n• 錯誤長這樣：{json}\n\n›\n")), None);
        assert_eq!(api_error_line(&format!("› hi\n• 錯誤長這樣：\n  ■ {json}\n\n›\n")), None);
        assert_eq!(api_error_line("› hi\n■ {\"status\":200,\"ok\":true}\n\n›\n"), None);
        assert_eq!(api_error_line("› hi\n■ {壞掉的 JSON\n\n›\n"), None);
    }

    /// 2026-10-17 w0:pDX：codex 的回覆被 OpenAI 資安過濾擋掉，畫面只剩 `ⓘ` 橫幅與縮排說明。照原畫面驗。
    #[test]
    fn a_codex_content_blocked_banner_is_a_turn_error() {
        let screen = "\
› let it 同併children多一點

• Ran sed -n '548,665p' crates/am-lifecycle/src/lifecycle/stop.rs
  └ pub async fn purge_bot_dir(
    +288 lines (^t to view transcript)

ⓘ This content can’t be shown
  We take extra care with some cybersecurity requests. If you’re doing authorized security work, apply for Daybreak to get broader access.
  Learn more: https://help.openai.com/en/articles/20001326
  Apply for Daybreak: https://openai.com/form/enterprise-trusted-access-for-cyber/

› Ask Codex to do anything

  GPT-6.1-Sol medium · ~/project/agents-manager · 優化 CICD 測試
";
        let line = api_error_line(screen).expect("資安過濾橫幅要命中");
        assert_eq!(line, CONTENT_BLOCKED);
        assert_eq!(kind_of(&line), "api_error");
        assert!(!is_quota_limit(&line));
        assert!(!is_quota_exhaustion(&line));
        assert_eq!(api_error_line("› hi\nⓘ This content can't be shown\n\n›\n").as_deref(), Some(CONTENT_BLOCKED));
    }

    /// 回覆談到這句話不算：`•` 回覆、縮排引文都不是行首的 `ⓘ`；橫幅之後又有回覆，代表已經過去了。
    #[test]
    fn a_reply_that_quotes_the_content_blocked_banner_is_not_an_error() {
        assert_eq!(api_error_line("› 這是什麼\n• 畫面上的 ⓘ This content can’t be shown 是資安過濾。\n\n›\n"), None);
        assert_eq!(api_error_line("› 這是什麼\n• 長這樣：\n  ⓘ This content can’t be shown\n  We take extra care…\n\n›\n"), None);
        let recovered = "› hi\nⓘ This content can’t be shown\n  Learn more: https://help.openai.com/\n\n› 改問別的\n• 好了，已經改完。\n\n›\n";
        assert_eq!(api_error_line(recovered), None);
    }

    #[test]
    fn a_reply_that_quotes_the_capacity_message_is_not_an_error() {
        let quoted = "› 這是什麼意思\n• 畫面上的 ■ Selected model is at capacity 表示模型滿載，請換模型。\n\n›\n";
        assert_eq!(api_error_line(quoted), None);
        let bullet = "› 解釋一下\n• Selected model is at capacity. Please try a different model.\n\n›\n";
        assert_eq!(api_error_line(bullet), None);
        let indented = "› 解釋一下\n• 這個錯誤長這樣：\n  ■ Selected model is at capacity. Please try a different model.\n\n›\n";
        assert_eq!(api_error_line(indented), None);
    }

    #[test]
    fn a_clean_turn_has_no_error() {
        let screen = "❯ echo 1\n⏺ PONG\n✻ Worked for 0s · done 1:07 AM\n──────\n❯\n";
        assert_eq!(api_error_line(screen), None);
        let prose = "❯ hi\n⏺ 這個 API Error 要自己處理\n✻ done\n❯\n";
        assert_eq!(api_error_line(prose), None);
        assert_eq!(api_error_line(""), None);
    }

    /// 2026-09-12 review h：以「API error」開頭的回覆行被誤釘成斷線。
    #[test]
    fn a_reply_line_that_merely_starts_with_api_error_is_not_a_banner() {
        let prose = "❯ 補上錯誤處理\n⏺ API error handling 已補上，測試也過了。\n✻ Worked for 9s · done 1:07 AM\n❯\n";
        assert_eq!(api_error_line(prose), None);
        let english = "❯ fix\n⏺ API errors are now retried three times.\n✻ done\n❯\n";
        assert_eq!(api_error_line(english), None);
        let banner = "❯ fix\n⏺ API Error: Request timed out.\n✻ done\n❯\n";
        assert_eq!(api_error_line(banner).as_deref(), Some("API Error: Request timed out."));
    }

    #[test]
    fn boxed_line_is_unwrapped() {
        let screen = "❯ hi\n│ ⏺ API Error: Connection lost mid-response. │\n│ ❯                                        │\n";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("API Error: Connection lost mid-response.")
        );
    }

    #[test]
    fn an_agy_individual_quota_banner_is_detected_from_the_screen() {
        let line = "Individual quota reached for Gemini Pro. Resets in 1h 30m";
        let screen = format!("❯ prompt\n{line}\n─────\n❯\n");
        assert_eq!(api_error_line(&screen).as_deref(), Some(line));
        assert!(is_quota_limit(line));
        assert!(is_quota_exhaustion(line));
        assert_eq!(agy_reset_until(line, "2026-10-05T10:00:00Z").as_deref(), Some("2026-10-05T11:30:00.000Z"));
    }

    /// 2026-10-24 實際畫面原文：`⚠` 前綴，`Resets in` 折到下一行。
    const AGY_WRAPPED_HEAD: &str = "⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in";
    const AGY_WRAPPED_TAIL: &str = "13m11s.";
    const AGY_JOINED: &str = "Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 13m11s.";

    #[test]
    fn an_agy_quota_banner_above_the_feedback_survey_is_still_the_turn_error() {
        let screen = format!(
            "❯ prompt\n{AGY_WRAPPED_HEAD}\n{AGY_WRAPPED_TAIL}\n How's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n? for shortcuts\n"
        );
        assert_eq!(api_error_line(&screen).as_deref(), Some(AGY_JOINED));
        let survey_only = "❯ prompt\n How's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n? for shortcuts\n";
        assert_eq!(api_error_line(survey_only), None);
    }

    #[test]
    fn a_wrapped_agy_banner_with_a_warning_glyph_is_detected_from_the_screen() {
        let screen = format!("❯ prompt\n{AGY_WRAPPED_HEAD}\n{AGY_WRAPPED_TAIL}\n─────\n❯\n");
        assert_eq!(api_error_line(&screen).as_deref(), Some(AGY_JOINED));
        assert!(is_quota_limit(AGY_JOINED));
        assert!(is_quota_exhaustion(AGY_JOINED));
        assert_eq!(kind_of(AGY_JOINED), "quota_limit");
        assert_eq!(agy_reset_until(AGY_JOINED, "2026-10-24T10:00:00Z").as_deref(), Some("2026-10-24T10:13:11.000Z"));
    }

    /// 合成樣本（依 agy 1.5.17 changelog 原文；無法在本機觸發空的預付錢包，未實錄）。
    #[test]
    fn an_agy_prepaid_balance_banner_is_quota_exhaustion() {
        let line = "Your organization's prepaid balance is insufficient. Contact your administrator to add funds.";
        let screen = format!("❯ prompt\n⚠ {line}\n─────\n❯\n");
        assert_eq!(api_error_line(&screen).as_deref(), Some(line));
        assert!(is_quota_limit(line));
        assert!(is_quota_exhaustion(line));
        assert_eq!(kind_of(line), "quota_limit");
        assert!(is_quota_exhaustion(&format!("RESOURCE_EXHAUSTED (code 8): {line}")));
        assert!(is_quota_exhaustion("You have exhausted your quota on this model."));
        assert!(!is_quota_exhaustion("The prepaid balance is insufficient, so I cannot run this query."));
    }

    #[test]
    fn agy_reset_durations_in_every_shape_are_read() {
        let at = "2026-10-24T10:00:00Z";
        for (tail, want) in [
            ("Resets in 3h 12m", "2026-10-24T13:12:00.000Z"),
            ("Resets in 3h12m.", "2026-10-24T13:12:00.000Z"),
            ("Resets in 4m 5s", "2026-10-24T10:04:05.000Z"),
            ("Resets in 13m11s.", "2026-10-24T10:13:11.000Z"),
            ("Resets in 45s", "2026-10-24T10:00:45.000Z"),
            ("Resets in 2h", "2026-10-24T12:00:00.000Z"),
            ("⚠ Individual quota reached. Resets in 1d 2h", "2026-10-25T12:00:00.000Z"),
            ("Resets in 3d 3h 59m", "2026-10-27T13:59:00.000Z"),
            ("Resets in 75h 59m", "2026-10-27T13:59:00.000Z"),
        ] {
            assert_eq!(agy_reset_until(tail, at).as_deref(), Some(want), "{tail}");
        }
    }

    #[test]
    fn an_unreadable_agy_reset_is_none_not_zero_or_now() {
        let at = "2026-10-24T10:00:00Z";
        for tail in ["Resets in", "Resets in soon", "Resets in 3", "Resets in 3x", "Resets in 0m", "Resets in 3h 12"] {
            assert_eq!(agy_reset_until(tail, at), None, "{tail}");
        }
    }

    #[test]
    fn an_agy_quota_line_that_is_not_a_hit_is_not_a_banner() {
        let quoted = format!("❯ 幫我查一下\n■ 你看到的是 Individual quota reached. Resets in 1h 30m\n─────\n❯\n");
        assert_eq!(api_error_line(&quoted), None);
        assert!(!is_quota_limit("Individual quota reached. Upgrade your plan."));
        let no_reset = "❯ x\nIndividual quota reached. Upgrade your plan.\n─────\n❯\n";
        assert_eq!(api_error_line(no_reset), None);
        let tail_only = "❯ x\n13m11s.\n─────\n❯\n";
        assert_eq!(api_error_line(tail_only), None);
        let not_a_duration = "❯ x\n⚠ Individual quota reached. Resets in\nPlease wait a while.\n─────\n❯\n";
        assert_eq!(api_error_line(not_a_duration), None);
        let survey_quoted_in_reply = "❯ x\nHow's the CLI experience so far? Help us improve:\n [1] Good  [2] Fine  [3] Bad  [0] Skip\n─────\n>\n─────\n? for shortcuts\n";
        assert_eq!(api_error_line(survey_quoted_in_reply), None);
        let prose = "The quota reached its limit; resets in spring.";
        assert!(!is_quota_limit(prose));
        assert!(!is_quota_exhaustion(prose));
        assert_eq!(api_error_line(&format!("❯ x\n{prose}\n─────\n❯\n")), None);
    }
}

/// 跟 codex 不同，**不**在下一回合成功時清掉：Fable 用盡後換 opus 照樣能跑，不代表 Fable 恢復；
/// 只靠 `until`（該桶子的 `resets_at`）到期解除。
///
/// 記不進去就回錯（#108 重開），不當作沒撞：讀不到這顆 bot 在哪台主機（不退回 `local`——那會把撞限寫進本機身份的
/// key）、那台的身份表還沒偵測完（key 算不準，[`crate::quota::resolve_quota_base`]）、排著的 prompt 身上的憑據寫不進去
/// （[`crate::lifecycle::quota_hold::stamp_queued`]）。撞限是外面已經發生的事，所以同時記成**欠著**（[`owed_limit_hit`]）：
/// 補上之前，這顆 bot 的 flush 與派送前都照欠著的那一筆擋；之後每一次問都先補一次。
pub async fn mark_claude_limit_hit(app: &impl TurnErrorContext, bot: &db::Bot, line: &str) -> Result<()> {
    if bot.kind != "claude" {
        return Ok(());
    }
    mark_limit_hit(app, bot, line, Banner::Claude).await
}

/// agy TUI 的 `Individual quota reached` 橫幅代表 5h 個人額度用盡，記在 agy 自己的 quota key。
pub async fn mark_agy_limit_hit(app: &impl TurnErrorContext, bot: &db::Bot, line: &str) -> Result<()> {
    if bot.kind != "agy" {
        return Ok(());
    }
    mark_limit_hit(app, bot, line, Banner::Agy).await
}

/// codex 的撞限橫幅（#198），規則同 [`mark_claude_limit_hit`]：讀不到這顆 bot 在哪台主機（以前退回 `local`——撞限寫進
/// 本機 `codex` 那一格，本機帳號被當成用盡、派工停擺，真正用盡的遠端身份反而沒擋）、那台的身份表還沒偵測完、排著的
/// prompt 蓋不上憑據，都回錯並記成欠著；補上之前 flush 與派送前照欠著的那一筆擋。
pub async fn mark_codex_limit_hit(app: &impl TurnErrorContext, bot: &db::Bot, notice: &str) -> Result<()> {
    match bot.kind.as_str() {
        "codex" => {
            let until = codex_banner_until(app, bot, notice).await;
            mark_limit_hit(app, bot, notice, Banner::Codex { until }).await
        }
        "grok" => {
            let (bucket, hours) = crate::lifecycle::grok_limit_window(notice);
            mark_limit_hit(app, bot, notice, Banner::Grok { bucket, hours }).await
        }
        _ => Ok(()),
    }
}

/// codex 橫幅上的時間是**那台主機**的當地時間（#239）：本機照 daemon 的時區；遠端用偵測時記下的 UTC 偏移。
/// 偏移讀不到就不猜（同 #59）：改用 app-server 讀到的重置時間，也沒有就撞限那一刻起 5 小時（最短的窗，寧可早放行再撞一次）。
async fn codex_banner_until(app: &impl TurnErrorContext, bot: &db::Bot, notice: &str) -> Option<String> {
    if !notice.to_ascii_lowercase().contains("try again at") {
        return None;
    }
    let host = db::bot_host(app.db(), &bot.id).await.ok();
    if host.as_deref() == Some(crate::config::LOCAL_HOST) {
        return crate::lifecycle::parse_codex_try_again(notice);
    }
    let offset = match &host {
        Some(h) => app.host_utc_offset_secs(h).await,
        None => None,
    };
    if let Some(until) = offset.and_then(|o| crate::lifecycle::parse_codex_try_again_offset(notice, o)) {
        return Some(until);
    }
    if let Some(until) = app.next_reset_for_bot(bot).await {
        return Some(until);
    }
    Some(db::iso_at(chrono::Utc::now() + chrono::Duration::hours(5)))
}

async fn mark_limit_hit(app: &impl TurnErrorContext, bot: &db::Bot, line: &str, banner: Banner) -> Result<()> {
    let identity = crate::quota::billing_identity(app, bot).await?;
    let (banner, at) = match owed(app, &bot.id).filter(|m| m.identity == identity && m.line == line) {
        Some(m) => (m.banner, m.at),
        None => (banner, db::now()),
    };
    let mark = Mark { banner, identity, line: line.to_string(), at, next_try: None };
    match record(app, &bot.id, &mark).await {
        Ok(()) => {
            settled(app, &bot.id, &mark);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, line, "a quota limit hit could not be recorded; it is owed and holds this bot");
            owe(app, &bot.id, mark);
            Err(e)
        }
    }
}

/// 一筆撞限：撞的那一刻、那時的身份、橫幅。欠著的時候原樣留著，補上時 `at` 不會變成補上的時間。
#[derive(Debug, Clone)]
struct Mark {
    banner: Banner,
    identity: Option<String>,
    line: String,
    at: String,
    /// 批次路徑（[`owed_limit_hit_throttled`]）補寫失敗之後，到這一刻之前不再補；`None`＝不節流。派送閘與 flush 不看這個欄位。
    next_try: Option<std::time::Instant>,
}

/// 撞限從哪一種 CLI 來：落在 `claude` 還是 `codex` 那一格，到期時間怎麼算。
#[derive(Debug, Clone, PartialEq)]
enum Banner {
    /// claude（`StopFailure`、畫面橫幅）：到期看那一桶的讀數，沒有就保底（[`fallback_until`]）。
    Claude,
    /// agy 的個人額度橫幅：five-hour 窗，通常直接讀 `Resets in …`。
    Agy,
    /// codex 的撞限橫幅：到期是橫幅上寫的時間，撞的當下解析（晚點補寫時裸鐘點可能已經過了）；沒寫就沒有
    /// （credits 用完，等下一個成功回合清）。
    Codex { until: Option<String> },
    /// grok 的撞額度畫面（#222）：桶名是額度窗（`seven_day`／`five_hour`，認不出就沒有），橫幅沒寫重置時間，
    /// 到期是撞的那一刻加保底時數（[`crate::lifecycle::grok_limit_window`]）。
    Grok { bucket: Option<&'static str>, hours: i64 },
}

impl Mark {
    /// 欠著的時候照這一筆擋。claude 還沒有那一桶的讀數可借，到期用保底時間。
    fn hit(&self) -> crate::quota::LimitHit {
        match &self.banner {
            Banner::Claude => {
                let lower = self.line.to_ascii_lowercase();
                crate::quota::LimitHit {
                    message: self.line.clone(),
                    until: fallback_until(&lower, &self.at),
                    at: self.at.clone(),
                    bucket: bucket_name(&lower),
                }
            }
            Banner::Agy => crate::quota::LimitHit {
                message: self.line.clone(),
                until: agy_reset_until(&self.line, &self.at).or_else(|| fallback_until("individual quota reached", &self.at)),
                at: self.at.clone(),
                bucket: Some("five_hour".into()),
            },
            Banner::Codex { until } => {
                crate::quota::LimitHit { message: self.line.clone(), until: until.clone(), at: self.at.clone(), bucket: None }
            }
            Banner::Grok { bucket, hours } => {
                let until = chrono::DateTime::parse_from_rfc3339(&self.at)
                    .ok()
                    .map(|t| crate::db::iso_at(t.with_timezone(&chrono::Utc) + chrono::Duration::hours(*hours)));
                crate::quota::LimitHit { message: self.line.clone(), until, at: self.at.clone(), bucket: bucket.map(String::from) }
            }
        }
    }
}

/// 寫進那台主機、那個身份的 key，再蓋到這顆 bot 排著的每一則上。
async fn record(app: &impl TurnErrorContext, bot_id: &str, m: &Mark) -> Result<()> {
    let db = DbContext::new(app.db().clone());
    let host = db::bot_host(db.pool(), bot_id).await?;
    let hit = match &m.banner {
        Banner::Claude => record_claude(app, &host, m).await?,
        Banner::Agy => record_agy(app, &host, m).await?,
        Banner::Codex { .. } => record_codex(app, &host, m).await?,
        Banner::Grok { .. } => record_grok(app, &host, m).await?,
    };
    crate::lifecycle::quota_hold::stamp_queued(app, bot_id, m.identity.as_deref(), &hit).await
}

/// agy 撞 5h 限額：把已讀到的 5h 窗標滿；讀數尚未進來時建立一個 100% 窗。
async fn record_agy<A: TurnErrorQuotaAccess>(access: &A, host: &str, m: &Mark) -> Result<crate::quota::LimitHit> {
    let key = access.resolve_key(host, "agy", m.identity.as_deref()).await?;
    let previous = access.snapshot(&key).await?;
    let hit = m.hit();
    if let Some(old) = previous.as_ref().and_then(|q| q.limit_hit.as_ref()) {
        let old = port_limit_to_daemon(old);
        if !crate::quota::limit_hit_expired(Some(&old)) && old.message == hit.message { return Ok(old); }
    }
    let reading_reset = previous.as_ref().and_then(|q| q.five_hour.as_ref()).and_then(|w| w.resets_at.as_deref()).filter(|reset| {
        chrono::DateTime::parse_from_rfc3339(reset).ok().is_some_and(|reset| chrono::DateTime::parse_from_rfc3339(&m.at).ok().is_some_and(|at| reset > at))
    }).map(str::to_string);
    let has_reading_reset = reading_reset.is_some();
    let mut hit = hit;
    hit.until = reading_reset.or(hit.until.clone());
    let mut quota = previous.unwrap_or_else(|| new_quota_snapshot(host, "agy-limit-hit", m.identity.clone()));
    match quota.five_hour.as_mut() {
        Some(window) => {
            window.used_pct = 100.0;
            window.observed_at = Some(m.at.clone());
            if !has_reading_reset { window.resets_at = hit.until.clone(); }
        }
        None => quota.five_hour = Some(PortWindow { observed_at: None, used_pct: 100.0, resets_at: hit.until.clone() }),
    }
    quota.limit_hit = Some(daemon_limit_to_port(&hit));
    quota.updated_at = db::now();
    access.store_snapshot(&key, quota).await?;
    Ok(hit)
}

async fn record_claude<A: TurnErrorQuotaAccess>(access: &A, host: &str, m: &Mark) -> Result<crate::quota::LimitHit> {
    let key = access.resolve_key(host, "claude", m.identity.as_deref()).await?;
    let mut quota = access.snapshot(&key).await?.unwrap_or_else(|| new_quota_snapshot(host, "claude-limit-hit", m.identity.clone()));
    let lower = m.line.to_ascii_lowercase();
    let until = saturate_bucket(&mut quota, &lower, &m.at).or_else(|| fallback_until(&lower, &m.at));
    let hit = crate::quota::LimitHit { message: m.line.clone(), until, at: m.at.clone(), bucket: bucket_name(&lower) };
    quota.limit_hit = Some(daemon_limit_to_port(&hit));
    quota.updated_at = db::now();
    access.store_snapshot(&key, quota).await?;
    Ok(hit)
}

/// grok 的一格（`grok`／`grok:<身份>`）：那個窗標成用完，撞限掛上去。窗沒有讀數時補一個 100% 的（重置時間留給
/// `/usage` 探測），額度面板才看得到「用完了」。
async fn record_codex<A: TurnErrorQuotaAccess>(access: &A, host: &str, m: &Mark) -> Result<crate::quota::LimitHit> {
    let key = access.resolve_key(host, "codex", m.identity.as_deref()).await?;
    let hit = m.hit();
    let mut quota = access.snapshot(&key).await?.unwrap_or_else(|| new_quota_snapshot(host, "codex-limit-hit", None));
    if let Some(previous) = quota.limit_hit.as_ref().filter(|previous| {
        previous.message == hit.message && !crate::quota::limit_hit_expired(Some(&port_limit_to_daemon(previous)))
    }) { return Ok(port_limit_to_daemon(previous)); }
    if let Some(window) = quota.five_hour.as_mut() {
        window.used_pct = 100.0;
    } else if let Some(window) = quota.seven_day.as_mut() {
        window.used_pct = 100.0;
    } else {
        quota.five_hour = Some(PortWindow { observed_at: None, used_pct: 100.0, resets_at: None });
    }
    quota.limit_hit = Some(daemon_limit_to_port(&hit));
    quota.updated_at = db::now();
    quota.source = "codex-limit-hit".into();
    access.store_snapshot(&key, quota).await?;
    Ok(hit)
}

async fn record_grok<A: TurnErrorQuotaAccess>(access: &A, host: &str, m: &Mark) -> Result<crate::quota::LimitHit> {
    let key = access.resolve_key(host, "grok", m.identity.as_deref()).await?;
    let previous = access.snapshot(&key).await?;
    let hit = m.hit();
    if let Some(h) = previous.as_ref().and_then(|q| q.limit_hit.as_ref()) {
        let previous_hit = port_limit_to_daemon(h);
        if !crate::quota::limit_hit_expired(Some(&previous_hit)) {
            let later = |a: &Option<String>, b: &Option<String>| match (a, b) {
                (Some(a), Some(b)) => chrono::DateTime::parse_from_rfc3339(a).ok() > chrono::DateTime::parse_from_rfc3339(b).ok(),
                (None, Some(_)) => true,
                _ => false,
            };
            if previous_hit.message == hit.message || !later(&hit.until, &previous_hit.until) { return Ok(previous_hit); }
        }
    }
    let mut quota = previous.unwrap_or_else(|| new_quota_snapshot(host, "grok-limit-hit", m.identity.clone()));
    let mark_full = |window: &mut Option<PortWindow>| match window {
        Some(window) => window.used_pct = 100.0,
        None => *window = Some(PortWindow { observed_at: None, used_pct: 100.0, resets_at: None }),
    };
    match hit.bucket.as_deref() {
        Some("seven_day") => mark_full(&mut quota.seven_day),
        Some("five_hour") => mark_full(&mut quota.five_hour),
        _ => {}
    }
    quota.limit_hit = Some(daemon_limit_to_port(&hit));
    quota.updated_at = db::now();
    access.store_snapshot(&key, quota).await?;
    Ok(hit)
}

fn new_quota_snapshot(host: &str, source: &str, account: Option<String>) -> QuotaSnapshot {
    QuotaSnapshot {
        five_hour: None,
        seven_day: None,
        fable: None,
        argon_weekly: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: db::now(),
        source: source.into(),
        account,
        host: host.to_string(),
    }
}

fn daemon_limit_to_port(hit: &crate::quota::LimitHit) -> PortLimitHit {
    PortLimitHit { message: hit.message.clone(), until: hit.until.clone(), at: hit.at.clone(), bucket: hit.bucket.clone() }
}

fn port_limit_to_daemon(hit: &PortLimitHit) -> crate::quota::LimitHit {
    crate::quota::LimitHit { message: hit.message.clone(), until: hit.until.clone(), at: hit.at.clone(), bucket: hit.bucket.clone() }
}

/// bot → 欠著的那一筆撞限（只留最新的）。只在記憶體：daemon 在補上之前重啟就沒了——`StopFailure` 那條路回錯、
/// 由 hook 收件匣（耐久）重試，那一回合也還沒收掉（在飛的回合本身就擋著 flush）；已經蓋上憑據的排隊 prompt 照
/// `quota_hold` 的規則擋。鍵帶 `boot_id`：測試裡模擬重啟的新 `App` 看不到上一個的帳，跟真的重啟一樣。
fn owed_marks() -> &'static std::sync::Mutex<std::collections::HashMap<String, Mark>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Mark>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn owed_key(app: &impl crate::capabilities::BootId, bot_id: &str) -> String {
    format!("{}\u{0}{bot_id}", app.boot_id())
}

fn owed(app: &impl crate::capabilities::BootId, bot_id: &str) -> Option<Mark> {
    owed_marks().lock().unwrap_or_else(|e| e.into_inner()).get(&owed_key(app, bot_id)).cloned()
}

/// 批次路徑補寫失敗後，同一筆欠帳隔多久才再試（`/api/state` 每次都會問，補不上就不該每次都重跑一整串查詢）。
const OWED_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// 補寫失敗：只在欠帳表裡還是同一筆（同身份、同撞限時刻）時記下 `next_try`；補的過程中被換成新的一筆就不動它。
fn back_off(app: &impl crate::capabilities::BootId, bot_id: &str, failed: &Mark) {
    let mut m = owed_marks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(x) = m.get_mut(&owed_key(app, bot_id)).filter(|x| x.identity == failed.identity && x.at == failed.at) {
        x.next_try = Some(std::time::Instant::now() + OWED_RETRY_BACKOFF);
    }
}

fn owe(app: &impl crate::capabilities::BootId, bot_id: &str, m: Mark) {
    owed_marks().lock().unwrap_or_else(|e| e.into_inner()).insert(owed_key(app, bot_id), m);
}

/// `recorded` 寫進去了：同一個身份、不比它新的那一筆欠帳一併結清（寫進的是同一把 key、同一刻或更晚的撞限）。
/// 補的過程中又欠了一筆更新的、或欠的是換身份之前那個身份的，留著。
fn settled(app: &impl crate::capabilities::BootId, bot_id: &str, recorded: &Mark) {
    let key = owed_key(app, bot_id);
    let mut m = owed_marks().lock().unwrap_or_else(|e| e.into_inner());
    if m.get(&key).is_some_and(|x| x.identity == recorded.identity && x.at <= recorded.at) {
        m.remove(&key);
    }
}

/// 這顆 bot 欠著、還擋著它的撞限：先補一次，補上就回 `None`（記憶體裡已經有了，照一般的查法）；補不上、而且那一筆
/// 是這顆 bot **現在**的身份撞的、沒到期、管得到它在跑的模型，就照那一筆擋。身份已經換掉的留著等補（寫回舊身份的
/// key），不擋新身份；到期的直接丟掉。`identity` 是呼叫端算好的 [`crate::quota::billing_identity`]（issue #238）。
pub async fn owed_limit_hit(app: &impl TurnErrorContext, bot: &db::Bot, identity: Option<&str>) -> Option<crate::quota::LimitHit> {
    owed_limit_hit_inner(app, bot, identity, false).await
}

/// [`owed_limit_hit`] 的批次版（只給 `GET /api/state` 的 `limit_hits_for_bots`）：補寫失敗後 [`OWED_RETRY_BACKOFF`] 內不再補
/// （跳過 `bot_host`／`resolve_key`／快照讀寫），其餘判斷照舊——照欠著的那一筆擋、身份與模型的檢查不變，回傳的值與
/// 不節流時一樣。補得上就照原本的路徑結清。
pub async fn owed_limit_hit_throttled(app: &impl TurnErrorContext, bot: &db::Bot, identity: Option<&str>) -> Option<crate::quota::LimitHit> {
    owed_limit_hit_inner(app, bot, identity, true).await
}

async fn owed_limit_hit_inner(app: &impl TurnErrorContext, bot: &db::Bot, identity: Option<&str>, throttle: bool) -> Option<crate::quota::LimitHit> {
    let m = owed(app, &bot.id)?;
    let hit = m.hit();
    if crate::quota::limit_hit_expired(Some(&hit)) {
        settled(app, &bot.id, &m);
        return None;
    }
    if !(throttle && m.next_try.is_some_and(|t| std::time::Instant::now() < t)) {
        match record(app, &bot.id, &m).await {
            Ok(()) => {
                tracing::info!(bot = %bot.id, "an owed quota limit hit is now recorded");
                settled(app, &bot.id, &m);
                return None;
            }
            Err(e) => {
                tracing::debug!(bot = %bot.id, error = %e, "an owed quota limit hit still cannot be recorded");
                if throttle {
                    back_off(app, &bot.id, &m);
                }
            }
        }
    }
    if m.identity.as_deref() != identity {
        return None;
    }
    let model = crate::quota::running_model(app, bot).await;
    crate::quota::limit_hit_blocks_model(&hit, model.as_deref()).then_some(hit)
}

#[cfg(feature = "test-hooks")]
pub fn owes_limit_hit(app: &impl crate::capabilities::BootId, bot_id: &str) -> bool {
    owed(app, bot_id).is_some()
}

#[cfg(all(test, not(feature = "test-hooks")))]
mod quota_limit_tests {
    use super::*;

    /// issue #108：`StopFailure` 的原因哪些算「帳號額度用完」。一下就好的限流不算——記成撞限會壓住派工好幾個小時。
    #[test]
    fn only_an_exhausted_account_counts_as_a_quota_stop_failure() {
        for yes in [
            "You've hit your session limit · resets 5pm",
            "API Error: You've hit your weekly limit · resets Sep 20",
            "You've reached your usage limit",
            "usage_limit_exceeded",
            "Claude usage limit reached. Your limit will reset at 5pm",
            "Individual quota reached for Gemini Pro. Resets in 1h 30m",
        ] {
            assert!(is_quota_exhaustion(yes), "{yes}");
        }
        for no in ["429 rate_limit", "overloaded_error", "API Error: 529 Overloaded", "You've hit your monthly spend limit", "API Error: 500"] {
            assert!(!is_quota_exhaustion(no), "{no}");
        }
    }

    /// 2026-11-25（#2200 同類）：`usage limit` 只在行首的橫幅形狀才算額度用完。grok 回覆與 hook 失敗細節裡的句中提到不算。
    #[test]
    fn a_mention_of_usage_limit_in_prose_is_not_an_exhaustion() {
        let grok_reply = "Here is how the usage limit works for SuperGrok.\nIf you see a usage limit warning, check /usage first.\n- usage limit reset: weekly";
        assert!(!is_quota_exhaustion(grok_reply));
        assert!(!is_quota_limit(grok_reply));
        for hook_detail in [
            "the request was rejected; the usage limit documentation is at docs.example",
            "retry policy: rate limit and usage limit errors are retried twice",
            "error while reading usage limit config: missing key",
            "API Error: usage limit exceeded for your team's monthly budget",
        ] {
            assert!(!is_quota_exhaustion(hook_detail), "{hook_detail}");
            assert!(!is_quota_limit(hook_detail), "{hook_detail}");
        }
        let long_prose = format!("You've hit your usage limit. {}", "x".repeat(300));
        assert!(!is_quota_exhaustion(&long_prose));
    }

    /// 真橫幅（行首、狀態列樣式前綴、`API Error:`／`ERROR:`）在 grok 回覆與 hook 細節裡都要認得。
    #[test]
    fn real_limit_banners_are_exhaustions_in_grok_replies_and_hook_details() {
        for banner in [
            "You've hit your usage limit. Try again at 6:43 PM.",
            "You've reached your usage limit",
            "■ You've hit your usage limit. … try again at 6:43 PM.",
            "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Aug 8th, 2025 1:47 PM.",
            "API Error: You've hit your weekly limit · resets Sep 20",
            "Claude usage limit reached. Your limit will reset at 5pm",
            "usage_limit_exceeded",
        ] {
            assert!(is_quota_exhaustion(banner), "{banner}");
            assert!(is_quota_exhaustion(&format!("Sure, here is the answer.\n{banner}\n")), "{banner}");
            assert_eq!(crate::hookrecv::classify_failure(Some(banner)), crate::hookrecv::FailureReason::RateLimit, "{banner}");
        }
        let grok_reply = "Summary of the run.\n⏺ You've reached your usage limit. Try again at 6:43 PM.\n";
        assert!(is_quota_exhaustion(grok_reply));
    }

    #[test]
    fn the_token_count_trailer_does_not_hide_the_limit_banner() {
        let screen = "❯ ping\n\nYou've reached your Fable limit. Run /usage-credits to continue or switch models with /model.\n\n0 tokens\n─────\n❯\n─────\n  tony. | agents-manager | Fable 5.1 | 5h:67% | F5:0%\n";
        assert_eq!(
            api_error_line(screen).as_deref(),
            Some("You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.")
        );
        assert!(is_token_count("0 tokens"));
        assert!(is_token_count("1,234 tokens"));
        assert!(!is_token_count("tokens"));
        assert!(!is_token_count("API error handling tokens"));
    }

    fn window(pct: f64, resets: &str) -> Option<PortWindow> {
        Some(PortWindow { observed_at: None, used_pct: pct, resets_at: Some(resets.into()) })
    }

    fn quota() -> QuotaSnapshot {
        QuotaSnapshot {
            five_hour: window(40.0, "5h-reset"),
            seven_day: window(60.0, "7d-reset"),
            fable: window(10.0, "fable-reset"),
            argon_weekly: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: String::new(),
            source: String::new(),
            account: None,
            host: "local".into(),
        }
    }

    /// 那一桶還沒有讀數時（daemon 剛重啟、statusline 還沒進來），撞限一樣要有出口：
    /// claude 沒有 `clear_limit_hit`，`until=None` 等於永遠卡在 `quota_blocked`。
    #[test]
    fn a_banner_with_no_window_reading_still_expires() {
        let at = "2026-09-16T10:00:00Z";
        for (line, want) in [
            ("You've hit your session limit", "2026-09-16T15:00:00.000Z"),
            ("You've hit your weekly limit", "2026-09-23T10:00:00.000Z"),
            ("You've hit your Fable limit", "2026-09-23T10:00:00.000Z"),
            ("You've hit your limit", "2026-09-16T15:00:00.000Z"),
        ] {
            assert_eq!(fallback_until(&line.to_ascii_lowercase(), at).as_deref(), Some(want), "{line}");
        }
        assert_eq!(fallback_until("session limit", "not-a-time"), None, "讀不懂時間就不要編一個出來");

        let mut q = QuotaSnapshot {
            five_hour: Some(PortWindow { observed_at: None, used_pct: 10.0, resets_at: Some("2026-09-16T12:00:00Z".into()) }),
            seven_day: None, fable: None, argon_weekly: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: at.into(), source: "test".into(), account: None, host: "local".into(),
        };
        let lower = "you've hit your session limit".to_string();
        let until = saturate_bucket(&mut q, &lower, at).or_else(|| fallback_until(&lower, at));
        assert_eq!(until.as_deref(), Some("2026-09-16T12:00:00Z"));
    }

    /// 前綴與桶名取自 2.1.273 binary 的字串表：`{five_hour:"session limit", seven_day:"weekly limit",
    /// seven_day_opus:"Opus limit", seven_day_sonnet:"Sonnet limit", seven_day_overage_included:"Fable limit"}`；
    /// `· resets …` 那段是示意，判斷不看它。以前非 Fable 一律記 5h——撞週額度卻把 5h 釘成 100%、而且等 5h 重置就當成解除了。
    #[test]
    fn each_banner_saturates_its_own_bucket() {
        for (line, bucket, until) in [
            ("You've hit your session limit · resets 4pm (Asia/Taipei)", "5h", "5h-reset"),
            ("You've hit your weekly limit · resets Sep 18", "7d", "7d-reset"),
            ("You've reached your Fable limit. Run /usage-credits to continue", "fable", "fable-reset"),
        ] {
            assert!(is_quota_limit(line), "{line}");
            let mut q = quota();
            assert_eq!(saturate_bucket(&mut q, &line.to_ascii_lowercase(), "2026-09-16T10:00:00Z").as_deref(), Some(until), "{line}");
            let pct = |w: &Option<PortWindow>| w.as_ref().unwrap().used_pct;
            let got = [("5h", pct(&q.five_hour)), ("7d", pct(&q.seven_day)), ("fable", pct(&q.fable))];
            for (name, p) in got {
                assert_eq!(p == 100.0, name == bucket, "{line}: {name}={p}");
            }
        }
    }

    /// review3 c4 M1：`Opus limit`／`Sonnet limit`（CLI 的 `seven_day_opus`／`seven_day_sonnet`）是模型自己的週桶。
    /// 以前記成 `seven_day` 並把 7d 量表釘成 100%：同帳號所有 claude bot 停派到週重置，群組任務換掉整個身份。
    #[test]
    fn a_model_limit_is_its_own_bucket_and_saturates_no_bar() {
        for (line, bucket) in [
            ("You've hit your Opus limit · resets Sep 18", "opus"),
            ("You've hit your Sonnet limit · resets Sep 18", "sonnet"),
        ] {
            let lower = line.to_ascii_lowercase();
            assert!(is_quota_limit(line), "{line}");
            assert_eq!(bucket_name(&lower).as_deref(), Some(bucket), "{line}");
            let mut q = quota();
            assert_eq!(saturate_bucket(&mut q, &lower, "2026-09-16T10:00:00Z").as_deref(), Some("7d-reset"), "{line}");
            let pct = |w: &Option<PortWindow>| w.as_ref().unwrap().used_pct;
            assert_eq!((pct(&q.five_hour), pct(&q.seven_day), pct(&q.fable)), (40.0, 60.0, 10.0), "{line}：量表不動");
            let mut empty = quota();
            empty.seven_day = None;
            let at = "2026-09-16T10:00:00Z";
            let until = saturate_bucket(&mut empty, &lower, at).or_else(|| fallback_until(&lower, at));
            assert_eq!(until.as_deref(), Some("2026-09-23T10:00:00.000Z"), "{line}");
        }
        assert_eq!(bucket_name("you've hit your weekly limit").as_deref(), Some("seven_day"));
    }

    /// 花費上限、fast 上限、團隊預算不是速率桶用完，不當撞限（否則量表被釘成 100%）。
    #[test]
    fn spend_and_fast_limits_are_not_rate_limits() {
        for line in [
            "You've hit your monthly spend limit.",
            "You've hit your fast limit",
            "You've hit your team's shared budget. Switch to another model",
        ] {
            assert!(!is_quota_limit(line), "{line}");
        }
    }

    /// issue #2200：claude 2.1.332 修掉「一般回覆以 You've used／You've hit your 開頭被顯示成紅色額度錯誤」。
    /// 這類回覆不是橫幅：不能被當成撞限（否則 bot 被擋、量表被釘）。真的橫幅仍要認得。
    #[test]
    fn an_ordinary_reply_that_starts_like_a_banner_is_not_a_quota_limit() {
        for reply in [
            "You've used 80% of your weekly limit so far.",
            "You've hit your weekly goal for the sprint, nice work!",
            "You've reached your goal; the limit was raised in the config.",
            "You've hit your session limit recovery plan is in the doc.",
        ] {
            assert!(!is_quota_limit(reply), "{reply}");
            assert!(!is_quota_exhaustion(reply), "{reply}");
        }
        let screen = "⏺ You've used 80% of your weekly limit so far.\n\n❯ \n  gpt-6-sol medium · ~/p · 5h 40% left · weekly 80% left\n";
        assert_eq!(api_error_line(screen), None);
        assert!(is_quota_limit("You've hit your session limit · resets 5pm"));
        assert!(is_quota_limit("You've reached your usage limit."));
    }

    #[test]
    fn fable_limit_banner_is_a_quota_limit_not_a_connection_error() {
        let line = "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.";
        assert!(is_api_error(line));
        assert!(is_quota_limit(line));
        assert!(!is_quota_limit("API Error: Connection lost mid-response. The response above may be incomplete."));
    }

    /// 2026-11 歷史實錄（DB 訊息節錄，去除個資）：撞限的錯誤細節漏判的形狀。
    /// codex 的橫幅用全形撇號 `’`、前綴 `ERROR:`；grok 是 `You hit your …` 沒有撇號；terminal 是 `Usage limit reached · …`；
    /// agy 的 gRPC 錯誤把橫幅包在 `RESOURCE_EXHAUSTED (code N): ` 後面。
    #[test]
    fn real_limit_details_from_codex_grok_terminal_and_agy_are_quota_exhaustion() {
        for detail in [
            "ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 3rd, 2:00 PM.",
            "ERROR: You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits.",
            "You hit your weekly limit.",
            "⚠ Usage limit reached · limit resets 11am (Asia/Taipei)",
            "Usage limit reached · continuing automatically at 3:00pm",
            "Usage limit reached again · continuing automatically at 3:00pm",
            "generating and executing: RESOURCE_EXHAUSTED (code 8): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 13m11s.",
            "generating and executing: transport error (ResourceExhausted, HTTP 429): RESOURCE_EXHAUSTED (code 8): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 13m11s.",
            "usage_limit_reached",
            "{\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"The usage limit has been reached. Your limit resets at 2026-11-25T10:00:00Z.\",\"resets_at\":1795000000,\"plan_type\":\"plus\",\"request_id\":\"req_0000000000000000000000000000000000000000000000000000000000000000\"}}",
        ] {
            assert!(is_quota_exhaustion(detail), "{detail}");
        }
    }

    /// 同一批實錄裡提到額度、但不是橫幅的句子：不算撞限。
    #[test]
    fn real_mentions_of_limits_in_prose_are_not_quota_exhaustion() {
        for mention in [
            "/upgrade to increase your usage limit.",
            "排進佇列等了 12 分鐘：目標身分沒有額度（ERROR: You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage）",
            "- **辨識不能放寬：** 必須同時看到「N-hour usage limit」和兩個選項才算是這個選單",
            "- **改法**：`is_quota_exhaustion` 改為逐行判。",
            "codex 畫面上顯示「You have 2 usage limit reset available」",
            "- 認出 usage_limit 代碼那條就好，句中提到不算",
        ] {
            assert!(!is_quota_exhaustion(mention), "{mention}");
        }
    }
}

