//! 對話裡 assistant 訊息的「翻譯」按鈕：`POST /api/messages/{id}/translate`（User-only）。
//!
//! 走共用的 [`crate::claude_api::complete`]（用途 `message_translate`：開關、key、保險絲、帳本都在那邊），
//! 不另寫 HTTP client。快取是 `{data_dir}/message-translate/{sha256(id + 內容)}.json`（0600，不進 SQLite）：
//! 同一則訊息、同一份內容翻過一次就不再扣費；內容變了（incomplete 補完）雜湊就變，會重翻。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::claude_api::{self, ClaudeError, ClaudeRequest};
use crate::config::CLAUDE_API_EFFORTS;
use crate::lc_error::LcError;
use crate::state::App;

const USE_CASE: &str = "message_translate";
const CACHE_DIR: &str = "message-translate";
const PROMPT_VERSION: &str = "3";
/// 訊息內容上限（字元）：再長的回覆不翻，避免一次扣太多。
pub(crate) const MAX_CHARS: usize = 20_000;
const MAX_OUT_TOKENS: u32 = 8_000;

const SYSTEM: &str = "You are a translation engine. Translate the English parts of the user's message into Traditional Chinese (zh-TW, Taiwan usage). \
Keep existing Traditional Chinese text, references, and names exactly as they are—do not rewrite or re-translate them. \
Convert Simplified Chinese to Traditional Chinese (zh-TW). Never output Japanese. \
Do NOT translate code, identifiers, file paths, shell commands, URLs, environment variables, commit hashes, version numbers, or issue numbers: copy them verbatim. \
Company names, product or system names, and abbreviations (e.g., CTBC, MIA02, UAT, PR) stay exactly as written in the source, like identifiers. \
Do not add explanations. Reply with the translation only.";

/// 品質沒過的細節：檢查名稱、拉丁／CJK 字數（同 `prose_stats`）、譯文與原文是否完全相同、每次嘗試用的模型與總次數。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub(crate) struct RejectDetail {
    pub check: String,
    pub src_latin: usize,
    pub src_cjk: usize,
    pub reply_latin: usize,
    pub reply_cjk: usize,
    pub identical: bool,
    pub models: Vec<String>,
    pub attempts: usize,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct CacheEntry {
    translation: String,
    model: String,
    prompt_version: String,
    /// 翻這次實際花的美元（依 [`cost_usd`]）；舊快取沒有。快取命中時照樣回傳，前端標「快取，未扣費」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    /// Jev 判定為忠實台灣繁體中文翻譯的機率（SPEC §4.3c 第六個場景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jev_prob: Option<f64>,
    /// 兩次嘗試都低於 `translate_min_probability` 時仍顯示的譯文：true，網頁標「品質分數偏低」。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    low_confidence: bool,
    /// 負面標記（品質檢查沒過）：`translation` 為空，`rejected` 是原因（`untranslated`／`japanese_output`；舊的 `low_fidelity` 已不擋）。
    /// 同一則訊息＋內容不再自動重翻重扣；手動重試（`force`）會略過它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejected_at: Option<String>,
    /// 品質沒過的那份譯文（最後一次嘗試）：使用者按「仍然顯示譯文」時用，不自動顯示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rejected_translation: Option<String>,
    /// 品質沒過的細節（給網頁「原因」與回報用）：只有數字與模型名，不含整段內容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reject_detail: Option<RejectDetail>,
    /// 回報給管理者的時間（10 分鐘內同一則只送一次）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_sent_at: Option<String>,
    /// 品質沒過後重試時用的 effort（往上提一階）；沒重試就沒有，照設定的 effort 翻。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

pub(crate) use crate::claude_api::cost_usd;

pub(crate) fn build_request(content: &str) -> ClaudeRequest {
    let prompt = format!(
        "Translate the English parts of the text inside <source> into Traditional Chinese (zh-TW, Taiwan usage). \
        原文中已經是中文（繁體）的段落、引用、名稱請原樣保留，不要改寫或重譯；簡體段落請轉成台灣繁體（zh-TW）。英文句子、條列項目與表格單元都要翻，但程式碼、指令、檔案路徑、版本號、產品名稱與縮寫照原文保留。\
        Reply with the translation only; do NOT include the <source> or </source> tags.\n<source>\n{content}\n</source>"
    );
    ClaudeRequest {
        use_case: USE_CASE,
        system: Some(SYSTEM.to_string()),
        prompt,
        max_tokens: MAX_OUT_TOKENS,
        format: None,
    }
}

/// 重試用的請求：上一次被擋時，在 prompt 裡直說錯在哪（2026-11-16：Haiku 對一則夾少量中文專有名詞的長英文回覆，兩次都原樣回傳原文；
/// 同樣的請求只提高 effort 不會改變結果，明確點出「上次回的跟原文一樣」才會翻）。
pub(crate) fn build_retry_request(content: &str, prev_reason: &str) -> ClaudeRequest {
    let mut req = build_request(content);
    let note = match prev_reason {
        "japanese_output" => "IMPORTANT: a previous attempt contained Japanese. Reply in Traditional Chinese (zh-TW) only. 前一次回覆含日文，請只用繁體中文。",
        _ => "IMPORTANT: a previous attempt returned the source unchanged (or still in English), which is wrong. Every English sentence, list item and table cell of prose must be rewritten in Traditional Chinese; only code, identifiers, paths and product names stay in English. 前一次回覆與原文相同，這是錯的：每一句英文都必須改寫成繁體中文。",
    };
    req.prompt = format!("{note}\n{}", req.prompt);
    req
}

pub(crate) fn clean_reply(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix("<source>").and_then(|s| s.strip_suffix("</source>")) {
        return inner.trim().to_string();
    }
    if let (Some(start), Some(end)) = (trimmed.find("<source>"), trimmed.rfind("</source>")) {
        if start < end {
            let inner = &trimmed[start + "<source>".len()..end];
            return inner.trim().to_string();
        }
    }
    trimmed.replace("<source>", "").replace("</source>", "").trim().to_string()
}

fn cache_key(id: &str, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(PROMPT_VERSION.as_bytes());
    h.update(b"\0");
    h.update(id.as_bytes());
    h.update(b"\0");
    h.update(content.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(CACHE_DIR).join(format!("{key}.json"))
}

fn load(path: &Path) -> Option<CacheEntry> {
    serde_json::from_slice(&std::fs::read(path).ok()?)
        .ok()
        .filter(|e: &CacheEntry| {
            !e.translation.trim().is_empty()
                && !is_mostly_english(&e.translation)
                && !has_japanese_kana(&e.translation)
        })
}

/// 讀負面標記；沒有或不是標記回 None。
fn load_rejection(path: &Path) -> Option<(String, Option<String>)> {
    let e: CacheEntry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    // `low_fidelity` 是舊版 Jev 門檻下的標記；現在低分也照顯示（`low_confidence`），舊標記不再擋，會自動重翻。
    let reason = e.rejected.filter(|r| r.as_str() != "low_fidelity" && e.translation.trim().is_empty())?;
    Some((reason, e.rejected_translation))
}

/// 命中快取的回傳內容。
fn hit_body(hit: &CacheEntry) -> Value {
    json!({
        "translation": hit.translation,
        "model": hit.model,
        "cached": true,
        "cost_usd": hit.cost_usd,
        "low_confidence": hit.low_confidence,
        "jev_prob": hit.jev_prob,
    })
}

/// 這個錯誤是「這則翻不好（品質檢查沒過）」，不是服務壞掉。
pub(crate) fn is_quality_rejection(e: &LcError) -> bool {
    matches!(e, LcError::Unprocessable(v) if v["error"] == "translate_rejected")
}

/// 422 本體：`reason` 是檢查名稱，`translation` 是被擋的那份譯文（網頁「仍然顯示譯文」用；舊標記沒有）。
fn rejection_error(reason: &str, translation: Option<&str>) -> LcError {
    rejection_error_with(reason, translation, None)
}

fn rejection_error_with(reason: &str, translation: Option<&str>, detail: Option<&RejectDetail>) -> LcError {
    let mut v = json!({"error": "translate_rejected", "reason": reason});
    if let Some(t) = translation.filter(|t| !t.trim().is_empty()) {
        v["translation"] = json!(t);
    }
    if let Some(d) = detail {
        v["detail"] = json!(d);
    }
    LcError::Unprocessable(v)
}

fn load_rejection_detail(path: &Path) -> Option<RejectDetail> {
    let e: CacheEntry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    e.reject_detail
}

/// 「主要是英文」判斷用的字數（Latin 字母數、CJK 字數；跟 [`is_mostly_english`] 同一套去程式碼的規則）。只有數字，不含內容，可進 log。
pub(crate) fn prose_stats(text: &str) -> (usize, usize) {
    let mut prose = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        prose.push_str(&rest[..i]);
        prose.push(' ');
        rest = match rest[i + 3..].find("```") { Some(j) => &rest[i + 3 + j + 3..], None => "" };
    }
    prose.push_str(rest);
    let (mut latin, mut cjk) = (0usize, 0usize);
    for line in prose.split('\n') {
        for (n, tok) in line.split('`').enumerate() {
            if n % 2 == 1 { continue }
            for w in tok.split_whitespace().filter(|w| !w.starts_with("http://") && !w.starts_with("https://")) {
                for c in w.chars() {
                    if c.is_ascii_alphabetic() { latin += 1 }
                    else if matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}') { cjk += 1 }
                }
            }
        }
    }
    (latin, cjk)
}

fn save(path: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    am_base::atomic_file::write(path, &data, am_base::atomic_file::Mode::Exact(0o600))
}

fn usage_json(u: &claude_api::Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_creation_input_tokens,
        "cache_read_input_tokens": u.cache_read_input_tokens,
    })
}

fn error_of(e: ClaudeError) -> LcError {
    match e {
        ClaudeError::Disabled | ClaudeError::NoKey(_) => LcError::conflict("claude_api_not_configured", json!({"message": e.to_string()})),
        // 翻譯設成走 OpenAI 但沒開或沒有 key：具名回報，不改走 Claude。
        ClaudeError::OpenAiNotConfigured(_) => LcError::conflict("openai_not_configured", json!({"message": "OpenAI 未啟用或未設定金鑰", "detail": e.to_string()})),
        other => LcError::Upstream(format!("translate_failed: {other}")),
    }
}

/// 自動翻譯（網頁的 `auto`，以及 daemon 的預先翻，見 `message_pretranslate`）是否開著：總開關、用途開關、`auto` 都要開。
pub(crate) async fn auto_on(app: &App) -> bool {
    let cfg = app.cfg.claude_fresh().await;
    cfg.claude_api.enabled && cfg.claude_use_state(USE_CASE).is_some_and(|s| s.enabled && s.auto == Some(true))
}

/// 核心：查快取、沒有才打 API、寫快取。回 `{translation, model, cached, usage?}`。
/// 簡體中文（[`crate::zh_convert::local_applies`]）不打 API：本地 OpenCC 轉成台灣繁體，寫同一份快取（model `opencc-s2twp`、費用 0）。
/// 同一個快取鍵同時只有一個上游呼叫（single-flight）：預先翻跟網頁的手動／自動翻撞在一起，後到的等先到的，拿到快取，不重複扣費。
pub(crate) async fn translate(app: &App, id: &str, content: &str) -> Result<Value, LcError> {
    translate_with(app, id, content, false).await
}

/// `force`：略過負面標記，重新打 API（使用者手動重試）。品質沒過回 422 `translate_rejected`（`reason`：`untranslated`／`low_fidelity`），並寫負面標記。
pub(crate) async fn translate_with(app: &App, id: &str, content: &str, force: bool) -> Result<Value, LcError> {
    let path = cache_path(&app.data_dir, &cache_key(id, content));
    if let Some(hit) = load(&path) {
        return Ok(hit_body(&hit));
    }
    if crate::zh_convert::local_applies(content) {
        return Ok(local_convert(content, &path));
    }
    if !force {
        if let Some((reason, tr)) = load_rejection(&path) {
            return Err(rejection_error_with(&reason, tr.as_deref(), load_rejection_detail(&path).as_ref()));
        }
    }
    let slot = flight_slot(&path);
    let guard = slot.lock().await;
    // 等鎖期間別人可能已經寫好快取：拿到控制權後重查，命中就不打 API。
    if let Some(hit) = load(&path) {
        drop(guard);
        flight_forget(&path, slot);
        return Ok(hit_body(&hit));
    }
    if !force {
        if let Some((reason, tr)) = load_rejection(&path) {
            drop(guard);
            flight_forget(&path, slot);
            return Err(rejection_error_with(&reason, tr.as_deref(), load_rejection_detail(&path).as_ref()));
        }
    }
    let out = translate_upstream(app, id, content, &path, force).await;
    drop(guard);
    flight_forget(&path, slot);
    out
}

/// 簡體 → 台灣繁體（本地，不呼叫 API）。結果寫進同一份快取，之後命中即 `cached:true`。
fn local_convert(content: &str, path: &Path) -> Value {
    let translation = crate::zh_convert::to_traditional(content);
    let entry = CacheEntry { translation: translation.clone(), model: crate::zh_convert::MODEL.into(), prompt_version: PROMPT_VERSION.into(), cost_usd: Some(0.0), jev_prob: None, ..Default::default() };
    if let Err(e) = save(path, &entry) {
        tracing::warn!("message translate cache write failed: {e}");
    }
    json!({"translation": translation, "model": crate::zh_convert::MODEL, "cached": false, "cost_usd": 0.0})
}

const JEV_CHARS: usize = 6_000;
const JEV_QUESTION: &str = "is_faithful";
const JEV_PROMPT: &str = "Does the TRANSLATION convey the same meaning as the SOURCE, written in Traditional Chinese (Taiwan)? Minor wording choices, translated or kept proper nouns, and omitted English glosses are acceptable; missing or added content is not.";

fn jev_request_body(model: &str, source: &str, translation: &str) -> Value {
    json!({
        "model": model,
        "state": {
            "source": source,
            "translation": translation,
        },
        "questions": {
            JEV_QUESTION: {
                "type": "noul",
                "instructions": {
                    "question": JEV_PROMPT,
                },
            }
        }
    })
}

/// 透過 Jev 檢查譯文是否為忠實的台灣繁體中文翻譯（SPEC §4.3c 第六個場景）。
/// 若 Jev 未啟用、未設定 key、保險絲用盡、逾時或發生錯誤，則 fail-open 記 debug log 並回傳 `None`。
async fn check_jev(app: &App, id: &str, source: &str, translation: &str) -> Option<f64> {
    let cfg = app.cfg.get().await.judge;
    if !cfg.enabled {
        return None;
    }
    let (bot_id, project_id, project_label, kind) = match sqlx::query_as::<_, (String, String, Option<String>, String)>(
        "SELECT b.id, b.project_id, p.label, b.kind FROM messages m \
         JOIN conversations c ON c.id = m.conversation_id \
         JOIN bots b ON b.id = c.bot_id \
         LEFT JOIN projects p ON p.id = b.project_id \
         WHERE m.id = ?"
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await
    {
        Ok(Some((b_id, p_id, p_label, kind))) => (b_id, p_id, p_label.unwrap_or_default(), kind),
        _ => (String::new(), "*".to_string(), String::new(), "claude".to_string()),
    };

    if crate::judge::gate(&cfg, &project_id, &project_label, 0).is_some() {
        return None;
    }

    let masked_source: String = crate::judge::mask(source).chars().take(JEV_CHARS).collect();
    let masked_translation: String = crate::judge::mask(translation).chars().take(JEV_CHARS).collect();
    let matched_line: String = masked_translation.chars().take(500).collect();

    let slot = match crate::judge::reserve_slot(
        app,
        &cfg,
        &project_id,
        &project_label,
        &bot_id,
        id,
        &kind,
        &matched_line,
        false,
        "translate_fidelity",
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("check_jev reserve_slot skipped or failed: {e}");
            return None;
        }
    };

    let body = jev_request_body(&cfg.model, &masked_source, &masked_translation);
    let started = std::time::Instant::now();
    let answer = match crate::judge::read_key(&cfg.key_file) {
        Ok(key) => crate::judge::ask_noul(&cfg, &key, &body, JEV_QUESTION).await,
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    let (p, model, tokens, error) = match &answer {
        Ok(a) => (Some(a.value), a.model.clone(), a.input_tokens, None),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    if let Err(e) = crate::judge::settle_slot(app, &slot, p, model, ms, tokens, error).await {
        tracing::debug!("check_jev settle_slot failed: {e}");
    }
    match answer {
        Ok(a) => Some(a.value),
        Err(e) => {
            tracing::debug!("check_jev ask_noul failed, fail open: {e}");
            None
        }
    }
}

/// 品質沒過：寫負面標記（覆蓋同鍵的舊檔），回 422。
fn mark_rejected(path: &Path, reason: &str, source: &str, reply: &str, models: Vec<String>, attempts: usize) -> LcError {
    // 只寫檢查名稱與數字（不含內容）：Latin／CJK 是去掉程式碼後的字母數，`identical` 是譯文與原文完全相同。
    let (sl, sc) = prose_stats(source);
    let (rl, rc) = prose_stats(reply);
    let detail = RejectDetail { check: reason.into(), src_latin: sl, src_cjk: sc, reply_latin: rl, reply_cjk: rc, identical: reply.trim() == source.trim(), models, attempts };
    let entry = CacheEntry {
        prompt_version: PROMPT_VERSION.into(),
        rejected: Some(reason.into()),
        rejected_at: Some(crate::db::now()),
        rejected_translation: Some(reply.to_string()).filter(|r| !r.trim().is_empty()),
        reject_detail: Some(detail.clone()),
        ..Default::default()
    };
    tracing::info!(check = reason, src_latin = sl, src_cjk = sc, reply_latin = rl, reply_cjk = rc, identical = (reply.trim() == source.trim()), src_chars = source.chars().count(), reply_chars = reply.chars().count(), "message translate rejected by quality check");
    if let Err(e) = save(path, &entry) {
        tracing::warn!("message translate rejection marker write failed: {e}");
    }
    rejection_error_with(reason, Some(reply), Some(&detail))
}

/// 品質沒過重試時的 effort：沿 [`CLAUDE_API_EFFORTS`] 往上一階（low → medium → high → xhigh → max）。
/// `current` 空字串或不認得＝模型預設 `high`，往上取一階（xhigh）；已經最高就停在 max。
pub(crate) fn next_effort(current: &str) -> &'static str {
    let ladder = CLAUDE_API_EFFORTS;
    let cur = current.trim();
    let high = ladder.iter().position(|e| *e == "high").unwrap_or(0);
    let idx = ladder.iter().position(|e| *e == cur).unwrap_or(high);
    ladder[(idx + 1).min(ladder.len() - 1)]
}

/// 同 [`next_effort`]，但照這個供應商自己的清單：OpenAI 是 none → low → medium → high → xhigh → max（不認得的當作預設 medium）。
pub(crate) fn next_effort_for(provider: &str, current: &str) -> &'static str {
    if provider != crate::config::API_PROVIDER_OPENAI {
        return next_effort(current);
    }
    let ladder = crate::config::OPENAI_USE_EFFORTS;
    let medium = ladder.iter().position(|e| *e == "medium").unwrap_or(0);
    let idx = ladder.iter().position(|e| *e == current.trim()).unwrap_or(medium);
    ladder[(idx + 1).min(ladder.len() - 1)]
}

/// 一次翻譯嘗試：譯文、本地檢查結果、Jev 分數（只有過本地檢查才問 Jev）。
struct Attempt {
    translation: String,
    model: String,
    usage: Option<claude_api::Usage>,
    cost: Option<f64>,
    /// 重試時用的 effort；第一次照設定的，沒有。
    effort: Option<String>,
    /// 本地檢查沒過的原因；過了就是 None。
    reason: Option<&'static str>,
    jev: Option<f64>,
}

/// `model`：`Some` 時這一次改用它（品質沒過的升級重試）；`None` 照設定的模型。
async fn attempt(app: &App, id: &str, content: &str, effort: Option<&str>, prev_reason: Option<&str>, model: Option<&str>) -> Result<Attempt, LcError> {
    let req = match prev_reason {
        Some(r) => build_retry_request(content, r),
        None => build_request(content),
    };
    let reply = match (model, effort) {
        (Some(m), e) => claude_api::complete_with_model(app, req, m, e).await,
        (None, Some(e)) => claude_api::complete_with_effort(app, req, e).await,
        (None, None) => claude_api::complete(app, req).await,
    }
    .map_err(error_of)?;
    let translation = clean_reply(&reply.text);
    let reason = local_reason(content, &translation);
    let jev = if reason.is_none() { check_jev(app, id, content, &translation).await } else { None };
    Ok(Attempt {
        cost: reply.usage.as_ref().and_then(|u| cost_usd(&reply.model, u)),
        model: reply.model,
        usage: reply.usage,
        effort: effort.map(str::to_string),
        translation,
        reason,
        jev,
    })
}

/// 要不要重試：本地檢查沒過，或 Jev 分數低於門檻。
fn needs_retry(a: &Attempt, min_prob: f64) -> bool {
    a.reason.is_some() || a.jev.is_some_and(|p| p < min_prob)
}

/// 挑選用的分數：Jev 不可用（None）視為通過（1.0）。
fn score(p: Option<f64>) -> f64 {
    p.unwrap_or(1.0)
}

async fn translate_upstream(app: &App, id: &str, content: &str, path: &Path, force: bool) -> Result<Value, LcError> {
    let min_prob = app.cfg.get().await.judge.translate_min_probability;
    let state = app.cfg.claude_fresh().await.claude_use_state(USE_CASE);
    let configured = state.as_ref().map(|s| s.effort.clone()).unwrap_or_default();
    let provider = state.as_ref().map_or(crate::config::API_PROVIDER_ANTHROPIC, |s| s.provider);
    // 升級模型（[claude_api.groups.translate] retry_model；空字串＝不升級）。手動重試（force）首次就用它。
    let retry_model = state.as_ref().map(|s| s.retry_model.trim().to_string()).unwrap_or_default();
    let first_model = (force && !retry_model.is_empty()).then_some(retry_model.as_str());
    let mut attempts = vec![attempt(app, id, content, None, None, first_model).await?];
    if needs_retry(&attempts[0], min_prob) {
        let prev = attempts[0].reason;
        // 本地檢查回 untranslated／japanese_output（Haiku 原樣回傳或夾日文）：重試改用升級模型，effort 照設定、不額外提高。
        // 已經用升級模型的（force 首次）就不再換，照舊只提高 effort。
        let upgrade = !retry_model.is_empty() && matches!(prev, Some("untranslated" | "japanese_output")) && attempts[0].model != retry_model;
        let next = if upgrade {
            attempt(app, id, content, None, prev, Some(&retry_model)).await?
        } else {
            // 只重試一次，effort 往上提一階（同樣的請求再打一次，模型通常還是給同樣的結果）。
            attempt(app, id, content, Some(next_effort_for(provider, &configured)), prev, None).await?
        };
        attempts.push(next);
    }

    // 通過本地檢查的嘗試裡，挑 Jev 分數最高的（同分取後一次）。都沒過本地檢查＝這則翻不好，回 422。
    let Some(best) = attempts.iter().filter(|a| a.reason.is_none()).max_by(|a, b| score(a.jev).total_cmp(&score(b.jev))) else {
        let last = attempts.last().expect("at least one attempt");
        let reason = last.reason.unwrap_or("untranslated");
        let models: Vec<String> = attempts.iter().map(|a| a.model.clone()).collect();
        return Err(mark_rejected(path, reason, content, &last.translation, models, attempts.len()));
    };

    // 費用是兩次嘗試的合計（重試也是真的扣費）。
    let costs: Vec<f64> = attempts.iter().filter_map(|a| a.cost).collect();
    let cost = (!costs.is_empty()).then(|| costs.iter().sum::<f64>());
    let low = best.jev.is_some_and(|p| p < min_prob);
    let entry = CacheEntry {
        translation: best.translation.clone(),
        model: best.model.clone(),
        prompt_version: PROMPT_VERSION.into(),
        cost_usd: cost,
        jev_prob: best.jev,
        low_confidence: low,
        effort: best.effort.clone(),
        ..Default::default()
    };
    if let Err(e) = save(path, &entry) {
        tracing::warn!("message translate cache write failed: {e}");
    }
    let mut body = json!({
        "translation": entry.translation,
        "model": entry.model,
        "cached": false,
        "cost_usd": cost,
        "low_confidence": low,
        "jev_prob": best.jev,
    });
    if let Some(e) = &entry.effort {
        body["effort"] = json!(e);
    }
    if let Some(u) = &best.usage {
        body["usage"] = usage_json(u);
    }
    Ok(body)
}

/// `POST /api/messages/{id}/translate`：只限 assistant 訊息。
pub async fn post_translate(
    State(app): State<Arc<App>>,
    AxPath(id): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<TranslateQuery>,
    Extension(principal): Extension<crate::api::RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    if principal != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let row: Option<(String, String)> = sqlx::query_as("SELECT role, content FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    let (role, content) = row.ok_or_else(|| LcError::NotFound("message".into()))?;
    if role != "assistant" {
        return Err(LcError::Bad("only assistant messages can be translated".into()));
    }
    if content.trim().is_empty() {
        return Err(LcError::Bad("empty message".into()));
    }
    if content.chars().count() > MAX_CHARS {
        return Err(LcError::Unprocessable(json!({"error": "message_too_long", "max_chars": MAX_CHARS})));
    }
    Ok(Json(translate_with(&app, &id, &content, q.force.unwrap_or(false)).await?))
}

#[derive(Deserialize, Default)]
pub struct TranslateQuery {
    /// 手動重試：略過品質沒過的負面標記。
    force: Option<bool>,
}

// ── 推播內文：一句短摘要（webpush.rs 用）──────────────────────────────────────────
//
// 與對話翻譯同一個用途（`message_translate`）與開關；譯文本身直接用 [`translate`]（快取跟網頁共用），
// 摘要只給推播用，快取在 `message-translate/ps-<sha256>.json`。

const SUMMARY_VERSION: &str = "1";
const SUMMARY_SYSTEM: &str = "You write one-line push notification text. Summarize the user's message as ONE short sentence in Traditional Chinese \
(zh-TW, Taiwan usage), at most 40 characters. 請寫成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。 \
Say what happened or what is being asked. Do NOT include code, file paths, URLs, \
commit hashes, keys or tokens. No quotes, no markdown, no preface. Reply with the sentence only.";
pub(crate) const SUMMARY_MAX_CHARS: usize = 40;

/// 判斷文字是否含有日文假名（平假名 U+3040–U+309F 或片假名 U+30A0–U+30FF）。
pub(crate) fn has_japanese_kana(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{3040}'..='\u{30FF}'))
}

/// 同一則訊息回報給管理者的最短間隔（秒）。
const REPORT_WINDOW_SECS: i64 = 600;

fn load_entry(path: &Path) -> Option<CacheEntry> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// `POST /api/messages/{id}/translate-report`（User-only）：翻譯品質沒過時，把原因摘要以 `bot_request` 送給本專案的管理者 bot。
/// 同一則訊息 10 分鐘內只送一次（送出時間記在快取的負面標記上，重啟也不會重送）。不含整段原文，只附前 200 字。
pub async fn post_translate_report(State(app): State<Arc<App>>, AxPath(id): AxPath<String>, Extension(principal): Extension<crate::api::RequestPrincipal>) -> Result<Json<Value>, LcError> {
    if principal != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT m.role, m.content, c.bot_id, b.project_id FROM messages m JOIN conversations c ON c.id = m.conversation_id JOIN bots b ON b.id = c.bot_id WHERE m.id = ?",
    )
    .bind(&id)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;
    let (role, content, bot_id, project_id) = row.ok_or_else(|| LcError::NotFound("message".into()))?;
    if role != "assistant" {
        return Err(LcError::Bad("only assistant messages can be reported".into()));
    }
    let path = cache_path(&app.data_dir, &cache_key(&id, &content));
    let mut entry = load_entry(&path).unwrap_or_default();
    let Some(reason) = entry.rejected.clone().filter(|_| entry.translation.trim().is_empty()) else {
        return Err(LcError::conflict("not_rejected", json!({"message_id": id})));
    };
    if let Some(sent) = entry.report_sent_at.as_deref() {
        let recent = chrono::DateTime::parse_from_rfc3339(sent).map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < REPORT_WINDOW_SECS).unwrap_or(false);
        if recent {
            return Ok(Json(json!({"sent": false, "reason": "duplicate", "sent_at": sent})));
        }
    }
    let manager: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM bots WHERE project_id = ? AND role = 'manager' AND parent_bot_id IS NULL AND deleted_at IS NULL ORDER BY created_at, id LIMIT 1",
    )
    .bind(&project_id)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?;
    let Some((manager_id,)) = manager else {
        return Err(LcError::conflict("no_manager", json!({"project_id": project_id})));
    };
    let detail = entry.reject_detail.clone();
    let summary = match &detail {
        Some(d) => format!(
            "翻譯品質檢查未通過：{}；原文拉丁 {}／CJK {}，譯文拉丁 {}／CJK {}，譯文與原文相同：{}，嘗試 {} 次，模型 {}",
            d.check, d.src_latin, d.src_cjk, d.reply_latin, d.reply_cjk, d.identical, d.attempts, d.models.join(" → ")
        ),
        None => format!("翻譯品質檢查未通過：{reason}"),
    };
    let payload = json!({
        "kind": "translate_report",
        "message_id": id,
        "bot_id": bot_id,
        "project_id": project_id,
        "reason": reason,
        "detail": detail,
        "summary": summary,
        "source_head": content.chars().take(200).collect::<String>(),
    });
    let event_key = format!("translate-report:{id}:{}", crate::db::ulid());
    crate::supervisor_inbox::push_inbox(&app.db, &event_key, "bot_request", None, Some(&manager_id), None, &payload)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    let sent_at = crate::db::now();
    entry.report_sent_at = Some(sent_at.clone());
    if let Err(e) = save(&path, &entry) {
        tracing::warn!("message translate report marker write failed: {e}");
    }
    Ok(Json(json!({"sent": true, "manager_bot_id": manager_id, "sent_at": sent_at})))
}

/// 本地廉價檢查：譯文出現日文假名（原文沒有）→ `japanese_output`；原文為英文且譯文仍主要是英文 → `untranslated`。都沒有回 None。
/// 混合訊息（英文段＋原樣保留的中文段）靠 `is_mostly_english` 的比例判斷：英文翻掉後譯文以中文為主，不會被判為 untranslated。
pub(crate) fn local_reason(source: &str, reply: &str) -> Option<&'static str> {
    if !has_japanese_kana(source) && has_japanese_kana(reply) {
        return Some("japanese_output");
    }
    if is_mostly_english(source) && is_mostly_english(reply) {
        return Some("untranslated");
    }
    None
}

/// 「主要是英文」：跟網頁 `isMostlyEnglish` 同一套規則（去掉程式碼區塊、行內 code、網址後，拉丁字母 ≥ 20 且 CJK 不超過十分之一）。
pub(crate) fn is_mostly_english(text: &str) -> bool {
    let mut prose = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        prose.push_str(&rest[..i]);
        prose.push(' ');
        rest = match rest[i + 3..].find("```") { Some(j) => &rest[i + 3 + j + 3..], None => "" };
    }
    prose.push_str(rest);
    let mut clean = String::new();
    for line in prose.split_inclusive('\n') {
        let mut in_code = false;
        for tok in line.split('`') {
            if !in_code { clean.push_str(tok) } else { clean.push(' ') }
            in_code = !in_code;
        }
        if line.ends_with('\n') { clean.push('\n') }
    }
    let words: Vec<&str> = clean.split_whitespace().filter(|w| !w.starts_with("http://") && !w.starts_with("https://")).collect();
    let (mut latin, mut cjk) = (0usize, 0usize);
    for c in words.iter().flat_map(|w| w.chars()) {
        if c.is_ascii_alphabetic() { latin += 1 }
        else if matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}') { cjk += 1 }
    }
    latin >= 20 && cjk * 10 <= latin
}

/// 推播用的一句摘要（≤ 40 字）。查快取、沒有才打 API。
pub(crate) async fn summarize(app: &App, content: &str) -> Result<String, LcError> {
    let mut h = Sha256::new();
    h.update(SUMMARY_VERSION.as_bytes());
    h.update(b"\0");
    h.update(content.as_bytes());
    let key = format!("ps-{}", h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>());
    let path = cache_path(&app.data_dir, &key);
    if let Some(hit) = load(&path) {
        return Ok(hit.translation);
    }
    let clipped: String = content.chars().take(MAX_CHARS).collect();
    let req = ClaudeRequest { use_case: USE_CASE, system: Some(SUMMARY_SYSTEM.to_string()), prompt: clipped, max_tokens: 200, format: None };
    let reply = claude_api::complete(app, req).await.map_err(error_of)?;
    let line = reply.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() {
        return Err(LcError::Upstream("summary_empty".into()));
    }
    let mut out: String = line.chars().take(SUMMARY_MAX_CHARS).collect();
    if line.chars().count() > SUMMARY_MAX_CHARS { out.push('…'); }
    let cost = reply.usage.as_ref().and_then(|u| cost_usd(&reply.model, u));
    let entry = CacheEntry { translation: out.clone(), model: reply.model, prompt_version: SUMMARY_VERSION.into(), cost_usd: cost, jev_prob: None, ..Default::default() };
    if let Err(e) = save(&path, &entry) {
        tracing::warn!("push summary cache write failed: {e}");
    }
    Ok(out)
}

// ── 待答問卷（codex／claude 的「需要回應」決策題）：一份問卷一次呼叫 ──────────────────────────
//
// 題目、標題、選項 label 與 description 結構化成 JSON 進、JSON 出，id 由 daemon 產生（`q{n}`、`q{n}-o{m}`）並逐一核對，
// 對不上就不採用、不寫快取。用途、開關、帳本、快取目錄都跟對話訊息翻譯共用；快取鍵是 `pq-` 前綴＋整份內容的雜湊。

const PQ_CACHE_PREFIX: &str = "pq-";
const PQ_PROMPT_VERSION: &str = "1";
/// 整份問卷的字元數上限（題目、標題、label、description 合計）：再長不翻，避免一次扣太多。
const PQ_MAX_CHARS: usize = 12_000;
const PQ_MAX_QUESTIONS: usize = 8;
const PQ_MAX_OUT_TOKENS: u32 = 6_000;

const PQ_SYSTEM: &str = "You are a translation engine. The user message is a JSON questionnaire that an AI coding agent asks its operator. \
Translate every \"header\", \"question\", option \"label\" and option \"description\" into Traditional Chinese (zh-TW, Taiwan usage). \
Keep every \"id\" exactly as given, and keep the same questions and options in the same order. \
Do NOT translate code, identifiers, file paths, commands, URLs, environment variables, branch names, commit hashes or issue numbers: copy them verbatim. \
Empty strings stay empty. Do not add explanations. Reply with JSON only, matching the schema.";

#[derive(Deserialize)]
pub struct PendingQuestionTranslateBody {
    #[serde(default)]
    questions: Vec<PqIn>,
}

#[derive(Deserialize)]
struct PqIn {
    #[serde(default)]
    header: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Vec<PqOptIn>,
}

#[derive(Deserialize)]
struct PqOptIn {
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize, Deserialize)]
struct PqCacheEntry {
    questions: Value,
    model: String,
    prompt_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

fn pq_qid(qi: usize) -> String {
    format!("q{qi}")
}

fn pq_oid(qi: usize, oi: usize) -> String {
    format!("q{qi}-o{oi}")
}

fn pq_output_schema() -> Value {
    let option = json!({
        "type": "object", "additionalProperties": false, "required": ["id", "label", "description"],
        "properties": { "id": {"type": "string"}, "label": {"type": "string"}, "description": {"type": "string"} }
    });
    json!({
        "type": "object", "additionalProperties": false, "required": ["questions"],
        "properties": { "questions": { "type": "array", "items": {
            "type": "object", "additionalProperties": false, "required": ["id", "header", "question", "options"],
            "properties": {
                "id": {"type": "string"}, "header": {"type": "string"}, "question": {"type": "string"},
                "options": { "type": "array", "items": option }
            }
        } } }
    })
}

fn pq_payload(qs: &[PqIn]) -> Value {
    let questions: Vec<Value> = qs
        .iter()
        .enumerate()
        .map(|(qi, q)| {
            let options: Vec<Value> = q
                .options
                .iter()
                .enumerate()
                .map(|(oi, o)| json!({"id": pq_oid(qi, oi), "label": o.label, "description": o.description}))
                .collect();
            json!({"id": pq_qid(qi), "header": q.header, "question": q.question, "options": options})
        })
        .collect();
    json!({ "questions": questions })
}

fn pq_chars(qs: &[PqIn]) -> usize {
    qs.iter()
        .map(|q| q.header.chars().count() + q.question.chars().count() + q.options.iter().map(|o| o.label.chars().count() + o.description.chars().count()).sum::<usize>())
        .sum()
}

/// 模型回的字串：沒有就不採用；原文有字而譯文空白也不採用。
fn pq_str(o: &Value, key: &str, orig: &str) -> Result<String, String> {
    let s = o.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing {key}"))?.trim().to_string();
    if s.is_empty() && !orig.trim().is_empty() {
        return Err(format!("empty {key}"));
    }
    Ok(s)
}

/// 核對模型回的 JSON：題數、選項數、每個 id 都要跟送出去的一樣，順序也一樣。回傳整理過的 `questions` 陣列。
fn pq_check(text: &str, qs: &[PqIn]) -> Result<Value, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad_json: {e}"))?;
    let out = v.get("questions").and_then(Value::as_array).ok_or("missing questions")?;
    if out.len() != qs.len() {
        return Err("question count mismatch".into());
    }
    let mut res = Vec::with_capacity(qs.len());
    for (qi, (q, o)) in qs.iter().zip(out).enumerate() {
        if o.get("id") != Some(&json!(pq_qid(qi))) {
            return Err("question id mismatch".into());
        }
        let header = pq_str(o, "header", &q.header)?;
        let question = pq_str(o, "question", &q.question)?;
        let opts_out = o.get("options").and_then(Value::as_array).ok_or("missing options")?;
        if opts_out.len() != q.options.len() {
            return Err("option count mismatch".into());
        }
        let mut opts = Vec::with_capacity(q.options.len());
        for (oi, (po, oo)) in q.options.iter().zip(opts_out).enumerate() {
            if oo.get("id") != Some(&json!(pq_oid(qi, oi))) {
                return Err("option id mismatch".into());
            }
            opts.push(json!({
                "id": pq_oid(qi, oi),
                "label": pq_str(oo, "label", &po.label)?,
                "description": pq_str(oo, "description", &po.description)?,
            }));
        }
        res.push(json!({"id": pq_qid(qi), "header": header, "question": question, "options": opts}));
    }
    Ok(Value::Array(res))
}

fn load_pq(path: &Path) -> Option<PqCacheEntry> {
    let e: PqCacheEntry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (e.prompt_version == PQ_PROMPT_VERSION && e.questions.is_array()).then_some(e)
}

fn save_pq(path: &Path, entry: &PqCacheEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    am_base::atomic_file::write(path, &data, am_base::atomic_file::Mode::Exact(0o600))
}

fn pq_cache_path(data_dir: &Path, payload: &Value) -> PathBuf {
    let mut h = Sha256::new();
    h.update(PQ_PROMPT_VERSION.as_bytes());
    h.update(b"\0");
    h.update(payload.to_string().as_bytes());
    let key: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    data_dir.join(CACHE_DIR).join(format!("{PQ_CACHE_PREFIX}{key}.json"))
}

/// 同一個快取鍵同時只有一個上游呼叫（單句、對話訊息、問卷共用）：後到的排隊、拿到先算好的快取（single-flight）。
/// 鍵是快取檔路徑；沒有人在等的鎖會被收掉。
static FLIGHT: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = std::sync::Mutex::new(None);

fn flight_slot(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut g = FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Default::default).entry(path.to_path_buf()).or_default().clone()
}

fn flight_forget(path: &Path, slot: Arc<tokio::sync::Mutex<()>>) {
    let mut g = FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    // 表裡一份、我手上一份；多的就是還有人在排隊。
    if Arc::strong_count(&slot) <= 2 {
        if let Some(m) = g.as_mut() {
            if m.get(path).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                m.remove(path);
            }
        }
    }
}

/// 核心：查快取、沒有才一次打 API、核對後寫快取。回 `{questions, model, cached, cost_usd, usage?}`。
pub(crate) async fn translate_questions(app: &App, qs: &[PqIn]) -> Result<Value, LcError> {
    let payload = pq_payload(qs);
    let path = pq_cache_path(&app.data_dir, &payload);
    if let Some(hit) = load_pq(&path) {
        return Ok(json!({"questions": hit.questions, "model": hit.model, "cached": true, "cost_usd": hit.cost_usd}));
    }
    let slot = flight_slot(&path);
    let guard = slot.lock().await;
    // 等鎖期間別人可能已經寫好快取：拿到控制權後重查，命中就不打 API。
    if let Some(hit) = load_pq(&path) {
        drop(guard);
        flight_forget(&path, slot);
        return Ok(json!({"questions": hit.questions, "model": hit.model, "cached": true, "cost_usd": hit.cost_usd}));
    }
    let out = translate_questions_upstream(app, qs, &payload, &path).await;
    drop(guard);
    flight_forget(&path, slot);
    out
}

async fn translate_questions_upstream(app: &App, qs: &[PqIn], payload: &Value, path: &Path) -> Result<Value, LcError> {
    let req = ClaudeRequest {
        use_case: USE_CASE,
        system: Some(PQ_SYSTEM.to_string()),
        prompt: payload.to_string(),
        max_tokens: PQ_MAX_OUT_TOKENS,
        format: Some(pq_output_schema()),
    };
    let reply = claude_api::complete(app, req).await.map_err(error_of)?;
    let questions = pq_check(&reply.text, qs).map_err(|e| LcError::Upstream(format!("translate_failed: {e}")))?;
    let cost = reply.usage.as_ref().and_then(|u| cost_usd(&reply.model, u));
    let entry = PqCacheEntry { questions: questions.clone(), model: reply.model.clone(), prompt_version: PQ_PROMPT_VERSION.into(), cost_usd: cost };
    if let Err(e) = save_pq(path, &entry) {
        tracing::warn!("pending question translate cache write failed: {e}");
    }
    let mut body = json!({"questions": questions, "model": reply.model, "cached": false, "cost_usd": cost});
    if let Some(u) = &reply.usage {
        body["usage"] = usage_json(u);
    }
    Ok(body)
}

/// `POST /api/pending-questions/translate`：User-only。一份問卷一次呼叫。
pub async fn post_pending_question_translate(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<crate::api::RequestPrincipal>,
    Json(body): Json<PendingQuestionTranslateBody>,
) -> Result<Json<Value>, LcError> {
    if principal != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    if body.questions.is_empty() || body.questions.len() > PQ_MAX_QUESTIONS {
        return Err(LcError::Bad(format!("questions must be 1..={PQ_MAX_QUESTIONS}")));
    }
    let chars = pq_chars(&body.questions);
    if chars == 0 {
        return Err(LcError::Bad("empty questionnaire".into()));
    }
    if chars > PQ_MAX_CHARS {
        return Err(LcError::Unprocessable(json!({"error": "questionnaire_too_long", "max_chars": PQ_MAX_CHARS})));
    }
    Ok(Json(translate_questions(&app, &body.questions).await?))
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    #[test]
    fn cost_uses_list_prices_and_unknown_models_have_none() {
        let u = claude_api::Usage { input_tokens: Some(1_000_000), output_tokens: Some(1_000_000), cache_creation_input_tokens: Some(1_000_000), cache_read_input_tokens: Some(1_000_000) };
        // Haiku 5.5：0.10 + 0.50 + 0.125 + 0.01
        assert!((cost_usd("claude-haiku-5-5", &u).unwrap() - 0.735).abs() < 1e-9);
        // Sonnet 5.5：2 + 10 + 2.5 + 0.1（快取讀取 $0.10/MTok＝輸入價 0.05×，a7d53148e）
        assert!((cost_usd("claude-sonnet-5-5", &u).unwrap() - 14.6).abs() < 1e-9);
        assert_eq!(cost_usd("gpt-x", &u), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Fake {
        calls: AtomicUsize,
        seen: Mutex<Vec<(Option<String>, String, String)>>,
    }

    impl MessagesClient for Fake {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push((req.system.clone(), req.prompt.clone(), req.model.clone()));
            Box::pin(async {
                Ok(ClientOutcome {
                    text: "  你好 `code`  ".into(),
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    async fn app(enabled: bool) -> (Arc<App>, PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-msg-translate-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let key = dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-msg").unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        store
            .update(move |c| {
                c.claude_api.key_file = key;
                if enabled {
                    c.claude_api.enabled = true;
                    c.claude_api.uses.insert(USE_CASE.into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), ..Default::default() });
                }
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn fake(app: &Arc<App>) -> Arc<Fake> {
        let f = Arc::new(Fake { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()) });
        app.claude_api.set_client(f.clone());
        f
    }

    #[test]
    fn prompt_keeps_code_and_targets_zh_tw() {
        let r = build_request("Run `cargo test` in /tmp");
        assert_eq!(r.use_case, "message_translate");
        let sys = r.system.unwrap();
        assert!(sys.contains("Translate the English parts") && sys.contains("Keep existing Traditional Chinese text") && sys.contains("do not rewrite"));
        assert!(r.prompt.contains("<source>\nRun `cargo test` in /tmp\n</source>"));
        assert!(r.prompt.contains("原文中已經是中文（繁體）的段落、引用、名稱請原樣保留"));
        assert!(r.prompt.contains("不要改寫或重譯"));
        assert!(sys.contains("Never output Japanese") && sys.contains("version numbers") && sys.contains("Convert Simplified Chinese"));
        assert!(r.prompt.contains("簡體段落請轉成台灣繁體") && r.prompt.contains("版本號") && r.prompt.contains("指令"));
    }

    #[test]
    fn mixed_message_is_not_untranslated() {
        // 英文為主、夾一段中文引用：英文翻掉、引用原樣保留 → 不判 untranslated
        let source = "Please review the deployment plan for the API service before Friday, the team needs it.\n這是引用：OK。";
        let translated = "請在週五前檢視 API 服務的部署計畫，團隊需要它。\n這是引用：OK。";
        assert_eq!(local_reason(source, translated), None, "英文已翻、中文引用保留的混合訊息不應判為 untranslated");
        // 英文原樣沒翻（只有中文引用保留）→ 仍要判 untranslated
        let untranslated = "Please review the deployment plan for the API service before Friday, the team needs it.\n這是引用：OK。";
        assert_eq!(local_reason(source, untranslated), Some("untranslated"), "英文沒翻的混合訊息仍要擋下");
    }

    #[test]
    fn mixed_message_with_code_versions_and_zh_quote_is_not_untranslated() {
        // 英文段含行內程式碼與版本號、夾繁體中文段：英文翻掉、其餘原樣保留 → 不判 untranslated
        let source = "Please upgrade the `agm` CLI to v0.155.1 before the release review, then rerun the checks.\n原文引用：請先確認。";
        let translated = "請在發版審查前把 `agm` CLI 升級到 v0.155.1，然後重跑檢查。\n原文引用：請先確認。";
        assert_eq!(local_reason(source, translated), None);
        // 來源中文字數超過拉丁字母十分之一時不算「主要英文」，本來就不做 untranslated 檢查（既有門檻，此次未改）。
    }

    #[test]
    fn prompt_keeps_company_and_product_abbreviations() {
        let r = build_request("Ticket CTBC on MIA02 for UAT.");
        let sys = r.system.unwrap();
        assert!(sys.contains("CTBC") && sys.contains("MIA02") && sys.contains("UAT") && sys.contains("PR"));
        assert!(sys.contains("stay exactly as written in the source"));
        assert!(r.prompt.contains("照原文保留"));
    }

    #[test]
    fn clean_reply_strips_source_tags() {
        assert_eq!(clean_reply("<source>你好，這是測試。</source>"), "你好，這是測試。");
        assert_eq!(clean_reply("  <source>\n你好，這是測試。\n</source>  "), "你好，這是測試。");
        assert_eq!(clean_reply("翻譯如下：\n<source>你好</source>"), "你好");
        assert_eq!(clean_reply("你好，這是正常的翻譯。"), "你好，這是正常的翻譯。");
    }

    struct FakeSequence {
        calls: AtomicUsize,
        replies: Mutex<Vec<String>>,
        /// 每次請求的 body（`request_json`），給測 effort 用。
        bodies: Mutex<Vec<Value>>,
    }

    impl MessagesClient for FakeSequence {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.bodies.lock().unwrap().push(req.request_json());
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let replies = self.replies.lock().unwrap();
            let text = replies.get(idx).cloned().unwrap_or_else(|| "fallback".to_string());
            Box::pin(async move {
                Ok(ClientOutcome {
                    text,
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    fn fake_seq(app: &Arc<App>, replies: Vec<String>) -> Arc<FakeSequence> {
        let f = Arc::new(FakeSequence { calls: AtomicUsize::new(0), replies: Mutex::new(replies), bodies: Mutex::new(Vec::new()) });
        app.claude_api.set_client(f.clone());
        f
    }

    #[tokio::test]
    async fn echo_then_good_caches_good_result() {
        let (app, _dir) = app(true).await;
        let content = "The 504 comes from GitHub, not AG Man's own server. The check for new codex versions asks GitHub for the latest 100 codex releases.";
        let replies = vec![
            content.to_string(),
            "504 錯誤來自 GitHub，而不是 AG Man 自己的伺服器。".to_string(),
        ];
        let f = fake_seq(&app, replies);
        let a = translate(&app, "m_retry", content).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "echo 後重試，共打兩次");
        assert_eq!(a["cached"], json!(false));
        assert_eq!(a["translation"], json!("504 錯誤來自 GitHub，而不是 AG Man 自己的伺服器。"));
        let key = cache_key("m_retry", content);
        assert!(cache_path(&app.data_dir, &key).exists(), "第二次成功的繁中譯文被存入快取");

        let b = translate(&app, "m_retry", content).await.unwrap();
        assert_eq!(b["cached"], json!(true));
        assert_eq!(b["translation"], json!("504 錯誤來自 GitHub，而不是 AG Man 自己的伺服器。"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "命中快取不調用 API");
    }

    /// 2026-11-16 回歸：一則夾少量中文專有名詞的長英文回覆，模型兩次都原樣回傳原文。重試的請求必須直說「上次回的跟原文一樣」，
    /// 不能只把 effort 往上提（同樣的請求得到同樣的結果）；第一次請求維持原樣。
    #[tokio::test]
    async fn retry_prompt_names_the_previous_failure_when_the_source_was_echoed() {
        let (app, _dir) = app(true).await;
        let content = "The inventory sync finished for the warehouse and every pending return was moved to the new queue without errors today.";
        let f = fake_seq(&app, vec![content.to_string(), "庫存同步已完成，所有待處理的退貨都已順利移到新的佇列，沒有任何錯誤發生。".to_string()]);
        let ok = translate(&app, "m_echo_retry", content).await.unwrap();
        assert!(ok["translation"].as_str().unwrap().contains("庫存"));
        let bodies = f.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(!bodies[0].to_string().contains("previous attempt"), "第一次請求不帶更正說明");
        assert!(bodies[1].to_string().contains("previous attempt returned the source unchanged"), "重試的請求點出上次原樣回傳");
    }

    #[test]
    fn prose_stats_counts_only_prose_letters_and_cjk() {
        let t = "Hello world `ignored_identifier` 掃碼移庫\n```\ncode block words\n```\nhttps://example.com/xx done";
        assert_eq!(prose_stats(t), (14, 4), "Hello world done＝14 個 Latin，4 個 CJK；行內 code、區塊、網址不算");
    }

    #[tokio::test]
    async fn echo_twice_fails_and_does_not_cache() {
        let (app, _dir) = app(true).await;
        let content = "The 504 comes from GitHub, not AG Man's own server. The check for new codex versions asks GitHub for the latest 100 codex releases.";
        let replies = vec![
            content.to_string(),
            content.to_string(),
        ];
        let f = fake_seq(&app, replies);
        let err = translate(&app, "m_fail", content).await.unwrap_err();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "重試一次後失敗，共打兩次");
        assert!(matches!(&err, LcError::Unprocessable(v) if v["error"] == "translate_rejected" && v["reason"] == "untranslated"), "品質沒過回 422 untranslated: {err:?}");
        let key = cache_key("m_fail", content);
        let path = cache_path(&app.data_dir, &key);
        assert_eq!(load_rejection(&path).map(|(r, _)| r).as_deref(), Some("untranslated"), "寫入負面標記");
        assert_eq!(load_rejection(&path).and_then(|(_, t)| t).as_deref(), Some(content), "標記帶著被擋的譯文，給網頁「仍然顯示譯文」");
        assert!(matches!(&err, LcError::Unprocessable(v) if v["translation"] == content), "422 本體也帶被擋的譯文");
        assert!(load(&path).is_none(), "標記不是譯文");
        // 第二次同一則：直接回標記，不打 API。
        let err2 = translate(&app, "m_fail", content).await.unwrap_err();
        assert!(matches!(&err2, LcError::Unprocessable(v) if v["reason"] == "untranslated"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "命中負面標記不調用 API");
        // 手動重試（force）才會再打。
        let _ = translate_with(&app, "m_fail", content, true).await;
        assert_eq!(f.calls.load(Ordering::SeqCst), 3, "force 略過標記");
    }

    #[tokio::test]
    async fn cached_english_entry_is_treated_as_miss() {
        let (app, _dir) = app(true).await;
        let content = "The 504 comes from GitHub, not AG Man's own server. The check for new codex versions asks GitHub for the latest 100 codex releases.";
        let key = cache_key("m_bad", content);
        let path = cache_path(&app.data_dir, &key);

        let bad_entry = CacheEntry {
            translation: content.to_string(),
            model: "claude-haiku-5-5".into(),
            prompt_version: PROMPT_VERSION.into(),
            cost_usd: Some(0.000193),
            jev_prob: None,
            ..Default::default()
        };
        save(&path, &bad_entry).unwrap();
        assert!(path.exists(), "壞快取已寫入磁碟");
        assert!(load(&path).is_none(), "load() 必須將 translation 為英文的快取視為 miss");

        let replies = vec!["504 錯誤來自 GitHub，已重新翻譯為繁體中文。".to_string()];
        let f = fake_seq(&app, replies);
        let res = translate(&app, "m_bad", content).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "壞快取被當成 miss，因此重新打 API");
        assert_eq!(res["cached"], json!(false));
        assert_eq!(res["translation"], json!("504 錯誤來自 GitHub，已重新翻譯為繁體中文。"));
        let hit = load(&path).expect("繁體中文快取被正確存檔且可被 load");
        assert_eq!(hit.translation, "504 錯誤來自 GitHub，已重新翻譯為繁體中文。");
    }

    #[tokio::test]
    async fn japanese_output_retries_and_caches_good() {
        let (app, _dir) = app(true).await;
        let content = "The system has encountered an unexpected condition.";
        let replies = vec![
            "システムで予期しない状態が発生しました。".to_string(),
            "系統遇到了非預期的狀況。".to_string(),
        ];
        let f = fake_seq(&app, replies);
        let a = translate(&app, "m_ja", content).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "日文假名觸發重試，共打兩次");
        assert_eq!(a["cached"], json!(false));
        assert_eq!(a["translation"], json!("系統遇到了非預期的狀況。"));
        let key = cache_key("m_ja", content);
        assert!(cache_path(&app.data_dir, &key).exists(), "第二次成功的繁中譯文被存入快取");

        let b = translate(&app, "m_ja", content).await.unwrap();
        assert_eq!(b["cached"], json!(true));
        assert_eq!(b["translation"], json!("系統遇到了非預期的狀況。"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "命中快取不重複調用 API");
    }

    fn effort_of(body: &Value) -> Value {
        body["output_config"]["effort"].clone()
    }

    #[test]
    fn next_effort_climbs_one_tier_and_stays_at_max() {
        assert_eq!(next_effort("low"), "medium");
        assert_eq!(next_effort("medium"), "high");
        assert_eq!(next_effort("high"), "xhigh");
        assert_eq!(next_effort("xhigh"), "max");
        assert_eq!(next_effort("max"), "max", "已經最高就停在最高");
        assert_eq!(next_effort(""), "xhigh", "未設定＝模型預設 high，往上一階");
        assert_eq!(next_effort(" low "), "medium", "前後空白忽略");
    }

    #[tokio::test]
    async fn rejected_first_reply_retries_once_at_next_effort() {
        let (app, _dir) = app(true).await;
        // 這兩個測的是「不升級、同模型提一階」的路徑；升級模型另有測試（retry_model_tests）。
        app.cfg.update(|c| { c.claude_api.groups.insert("translate".into(), crate::config::ClaudeApiUseCfg { retry_model: Some(String::new()), ..Default::default() }); Ok(()) }).await.unwrap();
        let content = "The system has encountered an unexpected condition.";
        let f = fake_seq(&app, vec!["システムで予期しない状態が発生しました。".into(), "系統遇到了非預期的狀況。".into()]);
        let a = translate(&app, "m_effort_up", content).await.unwrap();

        let bodies = f.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "品質沒過只重試一次");
        assert_eq!(effort_of(&bodies[0]), json!("low"), "第一次照設定的 effort（message_translate 預設 low）");
        assert_eq!(effort_of(&bodies[1]), json!("medium"), "重試提一階");
        assert_eq!(a["effort"], json!("medium"));
        let hit = load(&cache_path(&app.data_dir, &cache_key("m_effort_up", content))).expect("重試成功存入快取");
        assert_eq!(hit.effort.as_deref(), Some("medium"));
    }

    #[tokio::test]
    async fn retry_at_top_effort_stays_max() {
        let (app, _dir) = app(true).await;
        // 這兩個測的是「不升級、同模型提一階」的路徑；升級模型另有測試（retry_model_tests）。
        app.cfg.update(|c| { c.claude_api.groups.insert("translate".into(), crate::config::ClaudeApiUseCfg { retry_model: Some(String::new()), effort: "max".into(), ..Default::default() }); Ok(()) }).await.unwrap();
        app.cfg
            .update(|c| {
                c.claude_api.uses.insert(USE_CASE.into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), effort: "max".into(), ..Default::default() });
                Ok(())
            })
            .await
            .unwrap();
        let content = "The system has encountered an unexpected condition.";
        let f = fake_seq(&app, vec!["システムで予期しない状態が発生しました。".into(), "系統遇到了非預期的狀況。".into()]);
        let a = translate(&app, "m_effort_top", content).await.unwrap();

        let bodies = f.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        assert_eq!(effort_of(&bodies[0]), json!("max"));
        assert_eq!(effort_of(&bodies[1]), json!("max"), "已經最高，重試仍是 max");
        assert_eq!(a["effort"], json!("max"));
    }

    #[tokio::test]
    async fn cached_japanese_entry_is_treated_as_miss() {
        let (app, _dir) = app(true).await;
        let content = "The system has encountered an unexpected condition.";
        let key = cache_key("m_bad_ja", content);
        let path = cache_path(&app.data_dir, &key);

        let bad_entry = CacheEntry {
            translation: "システムで予期しない状態が発生しました。".to_string(),
            model: "claude-haiku-5-5".into(),
            prompt_version: PROMPT_VERSION.into(),
            cost_usd: Some(0.000193),
            jev_prob: None,
            ..Default::default()
        };
        save(&path, &bad_entry).unwrap();
        assert!(path.exists(), "壞日文快取已寫入磁碟");
        assert!(load(&path).is_none(), "load() 必須將含假名的快取視為 miss");

        let replies = vec!["系統遇到了非預期的狀況。".to_string()];
        let f = fake_seq(&app, replies);
        let res = translate(&app, "m_bad_ja", content).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "日文快取被當成 miss，重新打 API");
        assert_eq!(res["cached"], json!(false));
        assert_eq!(res["translation"], json!("系統遇到了非預期的狀況。"));
        let hit = load(&path).expect("繁體中文快取被正確存檔且可被 load");
        assert_eq!(hit.translation, "系統遇到了非預期的狀況。");
    }

    async fn fake_jev_server(probs: Vec<f64>) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cl = calls.clone();
        let probs = Arc::new(Mutex::new(probs));
        let app = axum::Router::new().route("/", axum::routing::post(move || {
            let calls = calls_cl.clone();
            let probs = probs.clone();
            async move {
                let idx = calls.fetch_add(1, Ordering::SeqCst);
                let p = probs.lock().unwrap().get(idx).copied().unwrap_or(1.0);
                axum::Json(json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "is_faithful": {
                            "noul": p,
                        }
                    },
                    "usage": {"input_tokens": 42}
                }))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}"), calls)
    }

    async fn enable_jev(app: &Arc<App>, dir: &Path, endpoint: String) {
        let key_file = dir.join("jev_key");
        std::fs::write(&key_file, "secret-jev-key\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key_str = key_file.to_string_lossy().into_owned();
        app.cfg.update(move |c| {
            c.judge.enabled = true;
            c.judge.endpoint = endpoint;
            c.judge.key_file = key_str;
            c.judge.projects = vec!["*".into()];
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn jev_low_then_high_caches_second_translation() {
        let (app, dir) = app(true).await;
        let (endpoint, jev_calls) = fake_jev_server(vec![0.2, 0.9]).await;
        enable_jev(&app, &dir, endpoint).await;

        let content = "The server returned 504 Gateway Timeout.";
        let replies = vec![
            "伺服器回傳了 504 閘道超時。".to_string(),
            "伺服器回應 504 Gateway Timeout（閘道逾時）。".to_string(),
        ];
        let f = fake_seq(&app, replies);
        let res = translate(&app, "m_jev_low_high", content).await.unwrap();

        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "Haiku 打兩次");
        assert_eq!(jev_calls.load(Ordering::SeqCst), 2, "Jev 問兩次");
        assert_eq!(res["translation"], json!("伺服器回應 504 Gateway Timeout（閘道逾時）。"));
        assert_eq!(res["cached"], json!(false));

        let key = cache_key("m_jev_low_high", content);
        let path = cache_path(&app.data_dir, &key);
        let hit = load(&path).expect("第二次成功結果入快取");
        assert_eq!(hit.translation, "伺服器回應 504 Gateway Timeout（閘道逾時）。");
        assert_eq!(hit.jev_prob, Some(0.9), "快取內存入通過的 Jev 機率");
    }

    #[tokio::test]
    async fn jev_low_twice_shows_better_attempt_as_low_confidence() {
        let (app, dir) = app(true).await;
        let (endpoint, jev_calls) = fake_jev_server(vec![0.2, 0.3]).await;
        enable_jev(&app, &dir, endpoint).await;

        let content = "The server returned 504 Gateway Timeout.";
        let replies = vec![
            "伺服器回傳了 504。".to_string(),
            "伺服器逾時。".to_string(),
        ];
        let f = fake_seq(&app, replies);
        let res = translate(&app, "m_jev_low", content).await.unwrap();

        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "Haiku 打兩次");
        assert_eq!(jev_calls.load(Ordering::SeqCst), 2, "Jev 問兩次");
        assert_eq!(res["translation"], json!("伺服器逾時。"), "取 Jev 分數較高的那次（0.3）");
        assert_eq!(res["low_confidence"], json!(true));
        assert_eq!(res["jev_prob"], json!(0.3));
        assert_eq!(res["cached"], json!(false));

        let path = cache_path(&app.data_dir, &cache_key("m_jev_low", content));
        let hit = load(&path).expect("低分譯文仍存入快取");
        assert!(hit.low_confidence);
        assert_eq!(hit.jev_prob, Some(0.3));
        assert_eq!(load_rejection(&path), None, "低分不寫負面標記");

        let again = translate(&app, "m_jev_low", content).await.unwrap();
        assert_eq!(again["cached"], json!(true));
        assert_eq!(again["low_confidence"], json!(true));
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "命中快取不重打");
    }

    #[tokio::test]
    async fn japanese_output_twice_is_rejected_as_japanese_output() {
        let (app, _dir) = app(true).await;
        let content = "The system has encountered an unexpected condition.";
        let f = fake_seq(&app, vec!["システムで予期しない状態が発生しました。".into(), "システムエラーです。".into()]);
        let err = translate(&app, "m_ja_twice", content).await.unwrap_err();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "兩次都有假名");
        assert!(matches!(&err, LcError::Unprocessable(v) if v["error"] == "translate_rejected" && v["reason"] == "japanese_output"), "{err:?}");
        let path = cache_path(&app.data_dir, &cache_key("m_ja_twice", content));
        assert_eq!(load_rejection(&path).map(|(r, _)| r).as_deref(), Some("japanese_output"));
        assert!(load(&path).is_none());
    }

    #[tokio::test]
    async fn old_low_fidelity_marker_is_ignored_and_retranslated() {
        let (app, _dir) = app(true).await;
        let content = "The server returned 504 Gateway Timeout.";
        let path = cache_path(&app.data_dir, &cache_key("m_old_marker", content));
        save(
            &path,
            &CacheEntry { rejected: Some("low_fidelity".into()), rejected_at: Some(crate::db::now()), prompt_version: PROMPT_VERSION.into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(load_rejection(&path), None, "舊的 low_fidelity 標記不擋");

        let f = fake_seq(&app, vec!["伺服器回傳了 504 閘道超時。".into()]);
        let res = translate(&app, "m_old_marker", content).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "舊標記被忽略，重新打 API");
        assert_eq!(res["translation"], json!("伺服器回傳了 504 閘道超時。"));
        assert_eq!(res["cached"], json!(false));
    }

    #[tokio::test]
    async fn jev_unavailable_accepts_translation() {
        let (app, _dir) = app(true).await;
        // Jev 預設是 disabled (app.cfg.get().judge.enabled == false)
        assert!(!app.cfg.get().await.judge.enabled);

        let content = "The operation completed successfully.";
        let replies = vec!["操作已成功完成。".to_string()];
        let f = fake_seq(&app, replies);
        let res = translate(&app, "m_jev_unavail", content).await.unwrap();

        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "只打一次 Haiku");
        assert_eq!(res["translation"], json!("操作已成功完成。"));
        assert_eq!(res["cached"], json!(false));

        let key = cache_key("m_jev_unavail", content);
        let path = cache_path(&app.data_dir, &key);
        let hit = load(&path).expect("Jev 不可用時 fail-open 寫入快取");
        assert_eq!(hit.translation, "操作已成功完成。");
        assert_eq!(hit.jev_prob, None, "Jev 未執行時 jev_prob 為 None");
    }

    #[test]
    fn cache_key_depends_on_id_and_content() {
        assert_eq!(cache_key("a", "x"), cache_key("a", "x"));
        assert_ne!(cache_key("a", "x"), cache_key("b", "x"));
        assert_ne!(cache_key("a", "x"), cache_key("a", "y"));
    }

    #[tokio::test]
    async fn second_call_hits_the_cache_without_calling_the_api() {
        let (app, _dir) = app(true).await;
        let f = fake(&app);
        let a = translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!((a["cached"].clone(), a["translation"].clone()), (json!(false), json!("你好 `code`")));
        assert_eq!(a["usage"]["output_tokens"], 5);
        let b = translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!((b["cached"].clone(), b["translation"].clone(), b["model"].clone()), (json!(true), json!("你好 `code`"), json!("claude-haiku-5-5")));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        translate(&app, "m1", "hello world, edited").await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "changed content is translated again");
        let mode = std::fs::metadata(cache_path(&app.data_dir, &cache_key("m1", "hello world"))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn successful_call_is_appended_to_spend_log_and_counts_in_local_attempts() {
        let (app, dir) = app(true).await;
        fake(&app);
        translate(&app, "m1", "hello world").await.unwrap();
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let rows = crate::claude_api::read_spend_since(&dir, since);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].1 > 0);
        let atts = crate::claude_api_balance::query_local_attempts_since(&app, &am_base::db::iso_at(since)).await.unwrap();
        assert_eq!(atts.iter().map(|a| a.microusd).sum::<i64>(), rows[0].1, "餘額的本機帳含這筆");
        use std::os::unix::fs::PermissionsExt;
        let f = dir.join("claude-api/spend.jsonl");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(f.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        // 快取命中不重複記
        translate(&app, "m1", "hello world").await.unwrap();
        assert_eq!(crate::claude_api::read_spend_since(&dir, since).len(), 1);
    }

    #[tokio::test]
    async fn not_enabled_is_a_clear_409_and_sends_nothing() {
        let (app, _dir) = app(false).await;
        let f = fake(&app);
        let err = translate(&app, "m1", "hello").await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "claude_api_not_configured"), "{err:?}");
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn simplified_chinese_is_converted_locally_without_an_api_call() {
        let (app, _dir) = app(true).await;
        let f = fake(&app);
        let simp = "这个问题我们需要修复一下，修复之后请再运行测试，确认没有问题了再告诉我们。";
        let a = translate(&app, "m-simp", simp).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 0, "簡體不打 API");
        assert_eq!((a["translation"].clone(), a["model"].clone(), a["cached"].clone(), a["cost_usd"].clone()), (
            json!("這個問題我們需要修復一下，修復之後請再執行測試，確認沒有問題了再告訴我們。"),
            json!("opencc-s2twp"),
            json!(false),
            json!(0.0),
        ));
        let b = translate(&app, "m-simp", simp).await.unwrap();
        assert_eq!((b["cached"].clone(), b["translation"].clone()), (json!(true), a["translation"].clone()), "同一份快取");
        // 繁體與日文照舊：不是簡體，也不是英文，這支端點仍會照舊送 LLM（前端只對英文與簡體出按鈕）。
        let trad = translate(&app, "m-trad", "這個問題我們需要修復一下，修復之後請再執行測試。").await.unwrap();
        assert_eq!(trad["model"], json!("claude-haiku-5-5"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn route_rejects_non_assistant_and_unknown_messages() {
        let (app, _dir) = app(true).await;
        fake(&app);
        let call = |id: &str| post_translate(State(app.clone()), AxPath(id.into()), axum::extract::Query(TranslateQuery::default()), Extension(crate::api::RequestPrincipal::User));
        assert!(matches!(call("nope").await.unwrap_err(), LcError::NotFound(_)));
        let bot = Extension(crate::api::RequestPrincipal::Bot("x".into()));
        assert!(matches!(post_translate(State(app.clone()), AxPath("nope".into()), axum::extract::Query(TranslateQuery::default()), bot).await.unwrap_err(), LcError::Forbidden(_)));
    }

    struct FakeText {
        calls: AtomicUsize,
        text: String,
    }

    impl MessagesClient for FakeText {
        fn complete<'a>(&'a self, _req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self.text.clone();
            Box::pin(async move {
                Ok(ClientOutcome {
                    text,
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    fn fake_text(app: &Arc<App>, text: &str) -> Arc<FakeText> {
        let f = Arc::new(FakeText { calls: AtomicUsize::new(0), text: text.into() });
        app.claude_api.set_client(f.clone());
        f
    }

    fn pq_body() -> PendingQuestionTranslateBody {
        serde_json::from_value(json!({"questions": [
            {"header": "Scope", "question": "Which project's advisors should review?", "options": [
                {"label": "This project", "description": "Only this one"},
                {"label": "All projects", "description": ""}
            ]},
            {"header": "Window", "question": "How far back should \"recent changes\" go?", "options": [
                {"label": "Last 24 hours", "description": "`git log --since=1.day`"}
            ]}
        ]}))
        .unwrap()
    }

    const PQ_REPLY: &str = r#"{"questions":[
        {"id":"q0","header":"範圍","question":"哪個專案的顧問該審？","options":[
            {"id":"q0-o0","label":"這個專案","description":"只看這個"},
            {"id":"q0-o1","label":"所有專案","description":""}]},
        {"id":"q1","header":"時間","question":"「最近的改動」要往回看多久？","options":[
            {"id":"q1-o0","label":"最近 24 小時","description":"`git log --since=1.day`"}]}
    ]}"#;

    #[tokio::test]
    async fn questionnaire_is_one_call_mapped_by_id_and_cached() {
        let (app, _dir) = app(true).await;
        let f = fake_text(&app, PQ_REPLY);
        let a = translate_questions(&app, &pq_body().questions).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "整份問卷只打一次");
        assert_eq!(a["cached"], json!(false));
        assert_eq!(a["questions"][0]["id"], json!("q0"));
        assert_eq!(a["questions"][0]["options"][1]["id"], json!("q0-o1"));
        assert_eq!(a["questions"][0]["question"], json!("哪個專案的顧問該審？"));
        assert_eq!(a["questions"][1]["options"][0]["label"], json!("最近 24 小時"));
        assert_eq!(a["questions"][1]["options"][0]["description"], json!("`git log --since=1.day`"), "code 原樣");
        let b = translate_questions(&app, &pq_body().questions).await.unwrap();
        assert_eq!((b["cached"].clone(), b["questions"].clone()), (json!(true), a["questions"].clone()));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "同一份問卷命中快取不打 API");
        let mut changed = pq_body();
        changed.questions[0].question.push('!');
        translate_questions(&app, &changed.questions).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "內容變了就重翻");
    }

    /// 上游會卡住，直到測試放行（模擬兩個分頁同時送同一份問卷）。
    struct GatedText {
        calls: AtomicUsize,
        gate: Arc<tokio::sync::Semaphore>,
        text: String,
    }

    impl MessagesClient for GatedText {
        fn complete<'a>(&'a self, _req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self.text.clone();
            let gate = self.gate.clone();
            Box::pin(async move {
                let _permit = gate.acquire().await.expect("gate open");
                Ok(ClientOutcome {
                    text,
                    model: Some("claude-haiku-5-5".into()),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    #[tokio::test]
    async fn same_questionnaire_in_flight_makes_one_upstream_call() {
        let (app, _dir) = app(true).await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let f = Arc::new(GatedText { calls: AtomicUsize::new(0), gate: gate.clone(), text: PQ_REPLY.into() });
        app.claude_api.set_client(f.clone());
        let qs = pq_body().questions;
        let release = {
            let gate = gate.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                gate.add_permits(1);
            }
        };
        let (a, b, ()) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            async { tokio::join!(translate_questions(&app, &qs), translate_questions(&app, &qs), release) },
        )
        .await
        .expect("兩個請求都要完成（不能因為第二個也去打上游而卡住）");
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(f.calls.load(Ordering::SeqCst), 1, "同一份問卷同時只打一次上游");
        assert_eq!(a["questions"], b["questions"], "兩份回應一致");
        assert_eq!(usize::from(a["cached"] == json!(true)) + usize::from(b["cached"] == json!(true)), 1, "後到的那份拿到快取");
        // 只看這份問卷的鍵：FLIGHT 是行程共用的，別的測試（預先翻）可能同時在飛。
        let key = pq_cache_path(&app.data_dir, &pq_payload(&qs));
        assert!(FLIGHT.lock().unwrap().as_ref().is_none_or(|m| !m.contains_key(&key)), "沒人等了，鎖表要收乾淨");
        // 不同問卷照常各自打一次。
        let mut other = pq_body();
        other.questions[0].question.push('?');
        gate.add_permits(1);
        translate_questions(&app, &other.questions).await.unwrap();
        assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn mismatched_ids_are_rejected_and_not_cached() {
        let (app, _dir) = app(true).await;
        let bad = PQ_REPLY.replace("q1-o0", "q1-o9");
        let f = fake_text(&app, &bad);
        let err = translate_questions(&app, &pq_body().questions).await.unwrap_err();
        assert!(matches!(&err, LcError::Upstream(m) if m.contains("option id mismatch")), "{err:?}");
        let again = translate_questions(&app, &pq_body().questions).await;
        assert!(again.is_err());
        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "壞的回覆不寫快取，下次還會打");
    }

    #[tokio::test]
    async fn questionnaire_not_enabled_is_409_and_sends_nothing() {
        let (app, _dir) = app(false).await;
        let f = fake_text(&app, PQ_REPLY);
        let err = translate_questions(&app, &pq_body().questions).await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "claude_api_not_configured"), "{err:?}");
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn route_is_user_only_and_bounded() {
        let (app, _dir) = app(true).await;
        let f = fake_text(&app, PQ_REPLY);
        let bot = Extension(crate::api::RequestPrincipal::Bot("x".into()));
        let r = post_pending_question_translate(State(app.clone()), bot, Json(pq_body())).await;
        assert!(matches!(r.unwrap_err(), LcError::Forbidden(_)));
        let user = || Extension(crate::api::RequestPrincipal::User);
        let empty: PendingQuestionTranslateBody = serde_json::from_value(json!({"questions": []})).unwrap();
        assert!(matches!(post_pending_question_translate(State(app.clone()), user(), Json(empty)).await.unwrap_err(), LcError::Bad(_)));
        let long: PendingQuestionTranslateBody = serde_json::from_value(json!({"questions": [{"question": "x".repeat(PQ_MAX_CHARS + 1)}]})).unwrap();
        assert!(matches!(post_pending_question_translate(State(app.clone()), user(), Json(long)).await.unwrap_err(), LcError::Unprocessable(_)));
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        let ok = post_pending_question_translate(State(app.clone()), user(), Json(pq_body())).await.unwrap();
        assert_eq!(ok.0["cached"], json!(false));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod retry_model_tests {
    use super::*;
    use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 依序回覆 `replies`，並記下每一次請求實際用的模型與 effort（合成回覆，不打真 API）。
    struct Echo {
        calls: AtomicUsize,
        replies: Mutex<Vec<String>>,
        models: Mutex<Vec<String>>,
        efforts: Mutex<Vec<String>>,
    }

    impl MessagesClient for Echo {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.models.lock().unwrap().push(req.model.clone());
            self.efforts.lock().unwrap().push(req.effort.clone());
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self.replies.lock().unwrap().get(idx).cloned().unwrap_or_else(|| "fallback".into());
            let model = req.model.clone();
            Box::pin(async move {
                Ok(ClientOutcome {
                    text,
                    model: Some(model),
                    stop_reason: Some("end_turn".into()),
                    usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
                })
            })
        }
    }

    async fn app(enabled: bool) -> (Arc<App>, PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-msg-retry-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let key = dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-retry").unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        store
            .update(move |c| {
                c.claude_api.key_file = key;
                if enabled {
                    c.claude_api.enabled = true;
                    c.claude_api.uses.insert(USE_CASE.into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), ..Default::default() });
                }
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn echo(app: &Arc<App>, replies: Vec<String>) -> Arc<Echo> {
        let f = Arc::new(Echo { calls: AtomicUsize::new(0), replies: Mutex::new(replies), models: Mutex::new(Vec::new()), efforts: Mutex::new(Vec::new()) });
        app.claude_api.set_client(f.clone());
        f
    }

    const SRC: &str = "The system has encountered an unexpected condition.";
    const GOOD: &str = "系統遇到了非預期的狀況。";

    /// 原樣回傳原文（Haiku 的老毛病）→ 重試改用預設升級模型 sonnet，effort 照設定（low）不額外提高。
    #[tokio::test]
    async fn untranslated_retry_upgrades_to_the_default_retry_model_at_the_configured_effort() {
        let (app, _d) = app(true).await;
        let f = echo(&app, vec![SRC.into(), GOOD.into()]);
        let out = translate(&app, "m_up", SRC).await.unwrap();
        assert_eq!(*f.models.lock().unwrap(), vec!["claude-haiku-5-5".to_string(), "claude-sonnet-5-5".to_string()]);
        let efforts = f.efforts.lock().unwrap().clone();
        assert_eq!(efforts, vec!["low".to_string(), "low".to_string()], "升級重試的 effort 照設定，不額外提高");
        assert_eq!(out["model"], json!("claude-sonnet-5-5"), "回傳的模型是實際改用的那一個");
        assert_eq!(out["translation"], json!(GOOD));
    }

    /// `retry_model` 設成空字串＝不升級：照舊同模型、effort 往上一階。
    #[tokio::test]
    async fn an_empty_retry_model_does_not_upgrade() {
        let (app, _d) = app(true).await;
        app.cfg
            .update(|c| {
                c.claude_api.groups.insert("translate".into(), crate::config::ClaudeApiUseCfg { retry_model: Some(String::new()), ..Default::default() });
                Ok(())
            })
            .await
            .unwrap();
        let f = echo(&app, vec![SRC.into(), GOOD.into()]);
        translate(&app, "m_noup", SRC).await.unwrap();
        assert_eq!(*f.models.lock().unwrap(), vec!["claude-haiku-5-5".to_string(), "claude-haiku-5-5".to_string()]);
        assert_eq!(f.efforts.lock().unwrap()[1], "medium", "不升級時照舊往上提一階");
    }

    /// 手動重試（force）首次就用升級模型。
    #[tokio::test]
    async fn a_forced_manual_retry_uses_the_retry_model_from_the_first_attempt() {
        let (app, _d) = app(true).await;
        let f = echo(&app, vec![GOOD.into()]);
        let out = translate_with(&app, "m_force", SRC, true).await.unwrap();
        assert_eq!(*f.models.lock().unwrap(), vec!["claude-sonnet-5-5".to_string()]);
        assert_eq!(out["translation"], json!(GOOD));
    }

    /// 熱重載：改 config.toml 的 `retry_model`，不重啟就生效。
    #[tokio::test]
    async fn an_edited_retry_model_takes_effect_without_a_restart() {
        let (app, dir) = app(true).await;
        let path = dir.join("config.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n[claude_api.groups.translate]\nretry_model = \"claude-opus-5-5\"\n");
        std::fs::write(&path, text).unwrap();
        let at = std::time::SystemTime::now() + std::time::Duration::from_secs(9);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(at).unwrap();
        let f = echo(&app, vec![SRC.into(), GOOD.into()]);
        translate(&app, "m_hot", SRC).await.unwrap();
        assert_eq!(f.models.lock().unwrap()[1], "claude-opus-5-5");
    }

    /// 兩次都沒過：422 帶原因細節（檢查名稱、字數、是否相同、用過的模型與次數），細節也寫進負面標記。
    #[tokio::test]
    async fn a_rejection_carries_the_reason_detail_and_is_stored_with_it() {
        let (app, _d) = app(true).await;
        echo(&app, vec![SRC.into(), SRC.into()]);
        let err = translate(&app, "m_detail", SRC).await.unwrap_err();
        let LcError::Unprocessable(v) = err else { panic!("422 expected") };
        assert_eq!(v["reason"], json!("untranslated"));
        assert_eq!(v["detail"]["check"], json!("untranslated"));
        assert_eq!(v["detail"]["identical"], json!(true));
        assert_eq!(v["detail"]["attempts"], json!(2));
        assert_eq!(v["detail"]["models"], json!(["claude-haiku-5-5", "claude-sonnet-5-5"]));
        let path = cache_path(&app.data_dir, &cache_key("m_detail", SRC));
        assert_eq!(load_rejection_detail(&path).map(|d| d.attempts), Some(2));
    }

    /// 回報：User 才能呼叫；訊息必須是已被擋的 assistant；送給本專案的 manager bot；10 分鐘內同一則只送一次。
    #[tokio::test]
    async fn the_report_goes_to_the_projects_manager_once_per_ten_minutes_and_only_for_users() {
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "xl-bot").await;
        sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let conv = crate::db::ulid();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)").bind(&conv).bind(&bot.id).bind(crate::db::now()).execute(&e.app.db).await.unwrap();
        let content = SRC.repeat(3);
        let msg = crate::db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'assistant',?,'hook',?)")
            .bind(&msg).bind(&conv).bind(&content).bind(crate::db::now()).execute(&e.app.db).await.unwrap();
        // 把「已被擋」的負面標記寫好（用真的寫入函式，不打 API）。
        let path = cache_path(&e.app.data_dir, &cache_key(&msg, &content));
        let _ = mark_rejected(&path, "untranslated", &content, &content, vec!["claude-haiku-5-5".into(), "claude-sonnet-5-5".into()], 2);

        let first = post_translate_report(State(e.app.clone()), AxPath(msg.clone()), Extension(crate::api::RequestPrincipal::User)).await.unwrap().0;
        assert_eq!(first["sent"], json!(true));
        assert_eq!(first["manager_bot_id"], json!(bot.id));
        let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='bot_request' AND bot_id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(n.0, 1, "送進管理者的收件匣");

        let again = post_translate_report(State(e.app.clone()), AxPath(msg.clone()), Extension(crate::api::RequestPrincipal::User)).await.unwrap().0;
        assert_eq!((again["sent"].clone(), again["reason"].clone()), (json!(false), json!("duplicate")), "10 分鐘內只送一次");

        let bot_caller = post_translate_report(State(e.app.clone()), AxPath(msg.clone()), Extension(crate::api::RequestPrincipal::Bot(bot.id.clone()))).await;
        assert!(matches!(bot_caller, Err(LcError::Forbidden(_))), "bot 呼叫回 403");

        let plain = crate::db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'assistant','plain reply',?,?)")
            .bind(&plain).bind(&conv).bind("hook").bind(crate::db::now()).execute(&e.app.db).await.unwrap();
        let not_rejected = post_translate_report(State(e.app.clone()), AxPath(plain), Extension(crate::api::RequestPrincipal::User)).await;
        assert!(matches!(not_rejected, Err(LcError::Conflict(_))), "沒有被擋的譯文不能回報");
    }
}

/// 翻譯改用 OpenAI（`[claude_api.groups.translate] provider = "openai"`，openai-api 切片 F）。
/// 假的 loopback Responses server；key 是 sentinel，不打真的 API。
#[cfg(test)]
mod openai_translate_tests {
    use super::*;
    use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const OPENAI_KEY: &str = "sk-openai-SENTINEL-do-not-leak";
    const SRC: &str = "The system has encountered an unexpected condition.";
    const GOOD: &str = "系統遇到了非預期的狀況。";

    /// Claude 那一側：只數有沒有被呼叫（OpenAI 的測試裡必須是 0）。
    struct ClaudeSpy(AtomicUsize);
    impl MessagesClient for ClaudeSpy {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let model = req.model.clone();
            Box::pin(async move {
                Ok(ClientOutcome { text: GOOD.into(), model: Some(model), stop_reason: Some("end_turn".into()), usage: Some(Usage { input_tokens: Some(10), output_tokens: Some(5), cache_creation_input_tokens: None, cache_read_input_tokens: None }) })
            })
        }
    }

    enum Reply {
        Text(&'static str, Value),
        Status(u16, Value),
    }

    #[derive(Default)]
    struct Seen {
        bodies: Vec<Value>,
        auth: Vec<String>,
    }

    struct Server {
        endpoint: String,
        seen: Arc<Mutex<Seen>>,
        stop: tokio_util::sync::CancellationToken,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.cancel();
        }
    }

    fn usage(input: u64, cached: u64, write: u64, output: u64) -> Value {
        json!({"input_tokens": input, "input_tokens_details": {"cached_tokens": cached, "cache_write_tokens": write}, "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": input + output})
    }

    fn sse_text(text: &str, usage: &Value) -> Vec<u8> {
        let msg = json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]});
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": text}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": msg.clone()}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": [msg], "usage": usage}}),
        ];
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect::<String>().into_bytes()
    }

    async fn serve(replies: Vec<Reply>) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}/v1/responses", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let script = Arc::new(Mutex::new(replies.into_iter().collect::<std::collections::VecDeque<_>>()));
        let (seen_in, stop) = (seen.clone(), tokio_util::sync::CancellationToken::new());
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let (seen, script) = (seen_in.clone(), script.clone());
                async move {
                    {
                        let mut seen = seen.lock().unwrap();
                        seen.bodies.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        seen.auth.push(headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                    }
                    let (status, content_type, body) = match script.lock().unwrap().pop_front() {
                        Some(Reply::Text(text, usage)) => (200, "text/event-stream", sse_text(text, &usage)),
                        Some(Reply::Status(status, body)) => (status, "application/json", body.to_string().into_bytes()),
                        None => (500, "application/json", b"{}".to_vec()),
                    };
                    axum::response::Response::builder().status(status).header("content-type", content_type).body(axum::body::Body::from(body)).unwrap()
                }
            }),
        );
        let child = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).with_graceful_shutdown(async move { child.cancelled().await }).await;
        });
        Server { endpoint, seen, stop }
    }

    /// Claude API 開著、有 Claude 的 key；OpenAI 的 key 檔已寫好但 `[openai_api]` 與翻譯組的供應商由各測試自己設。
    async fn app() -> (Arc<App>, PathBuf, Arc<ClaudeSpy>, String) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-msg-openai-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let key = dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-openai-suite").unwrap().publish().unwrap();
        let openai_key = dir.join("openai-key");
        crate::key_file::stage_key(&openai_key.to_string_lossy(), OPENAI_KEY).unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        store
            .update(move |c| {
                c.claude_api.key_file = key;
                c.claude_api.enabled = true;
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
        let spy = Arc::new(ClaudeSpy(AtomicUsize::new(0)));
        app.claude_api.set_client(spy.clone());
        (app, dir, spy, openai_key.to_string_lossy().into_owned())
    }

    async fn use_openai(app: &Arc<App>, key_file: &str, enabled: bool) {
        let key_file = key_file.to_string();
        app.cfg
            .update(move |c| {
                c.openai_api.enabled = enabled;
                c.openai_api.key_file = key_file;
                c.claude_api.groups.insert("translate".into(), crate::config::ClaudeApiUseCfg { provider: "openai".into(), ..Default::default() });
                Ok(())
            })
            .await
            .unwrap();
    }

    fn spend_lines(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("claude-api").join("spend.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[tokio::test]
    async fn translation_goes_to_openai_and_is_booked_as_openai_at_the_settled_price() {
        let (app, dir, claude, key_file) = app().await;
        use_openai(&app, &key_file, true).await;
        let server = serve(vec![Reply::Text(GOOD, usage(1200, 200, 100, 80))]).await;
        app.claude_api.set_openai_endpoint(&server.endpoint);

        let out = translate(&app, "m_openai", SRC).await.unwrap();
        assert_eq!((out["translation"].clone(), out["model"].clone(), out["cached"].clone()), (json!(GOOD), json!("gpt-6-luna"), json!(false)));
        assert_eq!(claude.0.load(Ordering::SeqCst), 0, "Claude is never called for an OpenAI translation");

        // 請求：無狀態、沒有工具、instructions＝system、input＝一則 user 訊息、effort 照設定（OpenAI 的預設 low）。
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.bodies.len(), 1);
        let body = &seen.bodies[0];
        assert_eq!((body["model"].clone(), body["store"].clone(), body["stream"].clone()), (json!("gpt-6-luna"), json!(false), json!(true)));
        assert!(body.get("tools").is_none() && body.get("previous_response_id").is_none());
        assert_eq!(body["instructions"], json!(SYSTEM));
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["max_output_tokens"], json!(MAX_OUT_TOKENS));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert!(input[0]["content"][0]["text"].as_str().unwrap().contains(SRC));
        assert_eq!(seen.auth, [format!("Bearer {OPENAI_KEY}")]);
        drop(seen);

        // 金額＝OpenAI 價表的結算：普通輸入 900、快取讀 200、快取寫 100、輸出 80。
        let settled = crate::api_bot::openai::pricing::settle(
            "gpt-6-luna",
            &crate::api_bot::openai::pricing::Usage { input_tokens: 1200, cached_tokens: 200, cache_write_tokens: 100, cache_write_12h_tokens: 0, output_tokens: 80, reasoning_tokens: 0 },
            None,
        )
        .unwrap() as i64;
        assert_eq!(settled, (900_i64 * 100_000 + 200 * 10_000 + 100 * 125_000 + 80 * 500_000 + 999_999) / 1_000_000);
        assert_eq!(out["cost_usd"], json!(settled as f64 / 1e6));
        assert_eq!(out["usage"], json!({"input_tokens": 900, "output_tokens": 80, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 200}));

        // 帳本與花費明細都標 provider=openai；花費明細的金額就是結算值。
        let ledger = app.claude_api.ledger.snapshot(10);
        assert_eq!((ledger["rows"][0]["provider"].clone(), ledger["rows"][0]["model"].clone(), ledger["rows"][0]["ok"].clone()), (json!("openai"), json!("gpt-6-luna"), json!(true)));
        let spend = spend_lines(&dir);
        assert_eq!(spend.len(), 1);
        assert_eq!((spend[0]["provider"].clone(), spend[0]["model"].clone(), spend[0]["microusd"].clone(), spend[0]["use_case"].clone()), (json!("openai"), json!("gpt-6-luna"), json!(settled), json!(USE_CASE)));
        // OpenAI 的花費不算進 Claude 帳戶餘額的扣減。
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(crate::claude_api::read_spend_since(&dir, since).is_empty());
        assert_eq!(crate::claude_api::read_provider_spend_since(&dir, since, "openai").len(), 1);

        // key 不出現在回應、帳本、花費明細。
        for text in [out.to_string(), ledger.to_string(), serde_json::to_string(&spend).unwrap()] {
            assert!(!text.contains(OPENAI_KEY) && !text.contains("SENTINEL"), "{text}");
        }
    }

    /// `[openai_api]` 沒開、或開了但 key 檔不能用：具名錯誤 `openai_not_configured`，不送任何請求，也**不**退回 Claude。
    #[tokio::test]
    async fn openai_off_or_without_a_key_is_a_named_error_and_never_falls_back_to_claude() {
        for (enabled, key_ok) in [(false, true), (true, false)] {
            let (app, dir, claude, key_file) = app().await;
            let key_file = if key_ok { key_file } else { dir.join("no-such-key").to_string_lossy().into_owned() };
            use_openai(&app, &key_file, enabled).await;
            let server = serve(vec![Reply::Text(GOOD, usage(10, 0, 0, 5))]).await;
            app.claude_api.set_openai_endpoint(&server.endpoint);
            let err = translate(&app, "m_off", SRC).await.unwrap_err();
            let LcError::Conflict(v) = &err else { panic!("409 expected: {err:?}") };
            assert_eq!(v["reason"], json!("openai_not_configured"), "enabled={enabled} key_ok={key_ok}");
            assert_eq!(v["message"], json!("OpenAI 未啟用或未設定金鑰"));
            assert!(!v.to_string().contains(OPENAI_KEY));
            assert_eq!(claude.0.load(Ordering::SeqCst), 0, "no silent fallback to Claude");
            assert!(server.seen.lock().unwrap().bodies.is_empty(), "nothing was sent to OpenAI either");
            assert!(spend_lines(&dir).is_empty());
            assert_eq!(app.claude_api.ledger.snapshot(10)["rows"].as_array().unwrap().len(), 0, "not sent, not booked");
        }
    }

    /// 熱重載：手改 config.toml 把翻譯組切到 OpenAI，不重啟就生效；改回來也立刻回到 Claude。
    #[tokio::test]
    async fn switching_the_provider_in_config_toml_takes_effect_without_a_restart() {
        let (app, dir, claude, key_file) = app().await;
        let server = serve(vec![Reply::Text(GOOD, usage(100, 0, 0, 20))]).await;
        app.claude_api.set_openai_endpoint(&server.endpoint);
        let path = dir.join("config.toml");
        let original = std::fs::read_to_string(&path).unwrap();
        let touch = |text: &str, secs: u64| {
            std::fs::write(&path, text).unwrap();
            let at = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(at).unwrap();
        };

        translate(&app, "m_hot_1", SRC).await.unwrap();
        assert_eq!((claude.0.load(Ordering::SeqCst), server.seen.lock().unwrap().bodies.len()), (1, 0), "default: Claude");

        touch(&format!("{original}\n[claude_api.groups.translate]\nprovider = \"openai\"\n\n[openai_api]\nenabled = true\nkey_file = \"{key_file}\"\n"), 9);
        let out = translate(&app, "m_hot_2", SRC).await.unwrap();
        assert_eq!(out["model"], json!("gpt-6-luna"));
        assert_eq!((claude.0.load(Ordering::SeqCst), server.seen.lock().unwrap().bodies.len()), (1, 1), "now OpenAI");

        touch(&original, 18);
        let out = translate(&app, "m_hot_3", SRC).await.unwrap();
        assert_eq!(out["model"], json!("claude-haiku-5-5"));
        assert_eq!((claude.0.load(Ordering::SeqCst), server.seen.lock().unwrap().bodies.len()), (2, 1), "back to Claude");
    }

    /// 品質沒過的重試留在 OpenAI：沒設重試模型＝同模型、effort 照 OpenAI 的清單提高一級。
    #[tokio::test]
    async fn a_quality_retry_stays_on_openai_and_raises_the_effort_one_step() {
        let (app, _dir, claude, key_file) = app().await;
        use_openai(&app, &key_file, true).await;
        let server = serve(vec![Reply::Text(SRC, usage(100, 0, 0, 20)), Reply::Text(GOOD, usage(100, 0, 0, 30))]).await;
        app.claude_api.set_openai_endpoint(&server.endpoint);
        let out = translate(&app, "m_retry", SRC).await.unwrap();
        assert_eq!((out["translation"].clone(), out["model"].clone()), (json!(GOOD), json!("gpt-6-luna")));
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen.bodies.iter().map(|b| (b["model"].as_str().unwrap().to_string(), b["reasoning"]["effort"].as_str().unwrap().to_string())).collect::<Vec<_>>(),
            [("gpt-6-luna".to_string(), "low".to_string()), ("gpt-6-luna".to_string(), "medium".to_string())]);
        assert_eq!(claude.0.load(Ordering::SeqCst), 0);
        assert_eq!(next_effort_for("openai", "none"), "low");
        assert_eq!(next_effort_for("openai", "max"), "max");
        assert_eq!(next_effort_for("openai", ""), "high", "不認得＝預設 medium，往上一級");
        assert_eq!(next_effort_for("anthropic", "low"), next_effort("low"));
    }

    /// 回應沒有可信的用量、或上游拒絕：是錯誤，不記花費（不當成 0 元的成功），錯誤訊息裡沒有 key。
    #[tokio::test]
    async fn a_reply_without_usage_or_an_upstream_rejection_is_an_error_with_no_spend_and_no_key() {
        let (app, dir, claude, key_file) = app().await;
        use_openai(&app, &key_file, true).await;
        let echo_key = json!({"error": {"code": "invalid_api_key", "message": format!("Incorrect API key provided: {OPENAI_KEY}")}});
        let server = serve(vec![Reply::Text(GOOD, Value::Null), Reply::Text(GOOD, json!({"output_tokens": 5})), Reply::Status(401, echo_key)]).await;
        app.claude_api.set_openai_endpoint(&server.endpoint);
        for id in ["m_nousage", "m_noinput", "m_401"] {
            let err = translate(&app, id, SRC).await.unwrap_err();
            let text = format!("{err:?}");
            assert!(matches!(err, LcError::Upstream(_)), "{id}: {text}");
            assert!(!text.contains(OPENAI_KEY) && !text.contains("SENTINEL"), "{id}: {text}");
        }
        assert!(spend_lines(&dir).is_empty(), "no usage, no spend line");
        let ledger = app.claude_api.ledger.snapshot(10);
        let kinds: Vec<_> = ledger["rows"].as_array().unwrap().iter().map(|r| (r["error_kind"].as_str().unwrap_or("").to_string(), r["provider"].as_str().unwrap().to_string())).collect();
        assert_eq!(kinds, [("auth".to_string(), "openai".to_string()), ("bad_response".to_string(), "openai".to_string()), ("bad_response".to_string(), "openai".to_string())]);
        assert!(!ledger.to_string().contains(OPENAI_KEY));
        assert_eq!(claude.0.load(Ordering::SeqCst), 0);
    }

    /// 設定驗證：只有翻譯組能設 openai；openai 的模型／effort／重試模型要屬於 OpenAI；手改設定檔繞過 PUT 也在呼叫時擋下。
    #[tokio::test]
    async fn only_the_translate_group_may_use_openai_and_its_model_must_be_an_openai_one() {
        let (app, _dir, claude, key_file) = app().await;
        let put = |body: Value| {
            let app = app.clone();
            async move { crate::claude_api::put_settings(axum::extract::State(app), axum::Json(serde_json::from_value(body).unwrap())).await }
        };
        for bad in [
            json!({"groups": {"schedule_outcome": {"provider": "openai"}}}),
            json!({"groups": {"changelog_analysis": {"provider": "openai"}}}),
            json!({"groups": {"translate": {"provider": "gemini"}}}),
            json!({"groups": {"translate": {"provider": "openai", "model": "claude-haiku-5-5"}}}),
            json!({"groups": {"translate": {"provider": "openai", "retry_model": "claude-sonnet-5-5"}}}),
            json!({"groups": {"translate": {"provider": "openai", "effort": "ultra"}}}),
            json!({"groups": {"translate": {"provider": "anthropic", "model": "gpt-6-luna"}}}),
            json!({"uses": {"message_translate": {"provider": "openai"}}}),
        ] {
            let err = put(bad.clone()).await.unwrap_err();
            assert!(matches!(err, LcError::Bad(_)), "{bad}: {err:?}");
        }
        assert!(app.cfg.claude_fresh().await.claude_api.groups.get("translate").is_none_or(|g| g.provider.is_empty()), "a refused PUT changes nothing");

        // 合法的切換：模型／effort／重試模型換成 OpenAI 的預設；更新框翻譯留在 Claude（不把 OpenAI 的模型寫進它的設定）。
        let axum::Json(view) = put(json!({"groups": {"translate": {"provider": "openai"}}})).await.unwrap();
        let group = view["groups"].as_array().unwrap().iter().find(|g| g["id"] == "translate").unwrap().clone();
        assert_eq!((group["provider"].clone(), group["model"].clone(), group["effort"].clone(), group["retry_model"].clone(), group["openai_allowed"].clone()), (json!("openai"), json!("gpt-6-luna"), json!("low"), json!(""), json!(true)));
        assert_eq!(view["openai"]["available"], json!(false), "OpenAI 還沒開：設定頁要顯示成停用並附原因");
        assert!(view["openai"]["unavailable_reason"].as_str().is_some_and(|r| r.contains("OpenAI")));
        let cfg = app.cfg.claude_fresh().await;
        assert_eq!(cfg.claude_use_state("message_translate").unwrap().provider, "openai");
        let changelog = cfg.claude_use_state("changelog_translate").unwrap();
        assert_eq!((changelog.provider, changelog.model.as_str()), ("anthropic", "claude-haiku-5-5"));
        assert!(cfg.changelog_translate.model.starts_with("claude-"));

        // 切回 Claude：回到 Claude 的預設，設定檔不留 provider 欄位。
        let axum::Json(view) = put(json!({"groups": {"translate": {"provider": "anthropic"}}})).await.unwrap();
        let group = view["groups"].as_array().unwrap().iter().find(|g| g["id"] == "translate").unwrap().clone();
        assert_eq!((group["provider"].clone(), group["model"].clone(), group["retry_model"].clone()), (json!("anthropic"), json!("claude-haiku-5-5"), json!("claude-sonnet-5-5")));
        assert!(app.cfg.claude_fresh().await.claude_api.groups["translate"].provider.is_empty());

        // 手改設定檔：別的組寫了 openai，那個用途的呼叫回 bad_config，不送。
        app.cfg.update(|c| { c.claude_api.groups.insert("schedule_outcome".into(), crate::config::ClaudeApiUseCfg { enabled: Some(true), provider: "openai".into(), ..Default::default() }); Ok(()) }).await.unwrap();
        use_openai(&app, &key_file, true).await;
        let cfg = app.cfg.claude_fresh().await;
        assert_eq!(cfg.claude_use_state("schedule_outcome").unwrap().provider, "anthropic", "a group that may not use OpenAI never resolves to it");
        let err = crate::claude_api::complete_with(&cfg, &app.claude_api, ClaudeRequest { use_case: "schedule_outcome", system: None, prompt: "x".into(), max_tokens: 16, format: None }).await.unwrap_err();
        assert_eq!(err.kind(), "bad_config");
        assert_eq!(claude.0.load(Ordering::SeqCst), 0);
    }
}
