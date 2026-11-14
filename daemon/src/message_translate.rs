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
use crate::lc_error::LcError;
use crate::state::App;

const USE_CASE: &str = "message_translate";
const CACHE_DIR: &str = "message-translate";
const PROMPT_VERSION: &str = "1";
/// 訊息內容上限（字元）：再長的回覆不翻，避免一次扣太多。
pub(crate) const MAX_CHARS: usize = 20_000;
const MAX_OUT_TOKENS: u32 = 8_000;

const SYSTEM: &str = "You are a translation engine. Translate the user's message into Traditional Chinese (zh-TW, Taiwan usage). \
請翻譯成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。 \
Preserve the markdown structure exactly (headings, lists, tables, emphasis, links). \
Do NOT translate fenced code blocks, inline code, file paths, shell commands, URLs, identifiers, environment variables, \
commit hashes or issue numbers: copy them verbatim. Do not add explanations, notes or quotes. Reply with the translation only.";

#[derive(Serialize, Deserialize, Clone)]
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
}

pub(crate) use crate::claude_api::cost_usd;

pub(crate) fn build_request(content: &str) -> ClaudeRequest {
    let prompt = format!(
        "Translate the text inside <source> into Traditional Chinese (zh-TW, Taiwan usage). 請翻譯成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。Reply with the translation only; do NOT include the <source> or </source> tags in your reply.\n<source>\n{content}\n</source>"
    );
    ClaudeRequest {
        use_case: USE_CASE,
        system: Some(SYSTEM.to_string()),
        prompt,
        max_tokens: MAX_OUT_TOKENS,
        format: None,
    }
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
        other => LcError::Upstream(format!("translate_failed: {other}")),
    }
}

/// 自動翻譯（網頁的 `auto`，以及 daemon 的預先翻，見 `message_pretranslate`）是否開著：總開關、用途開關、`auto` 都要開。
pub(crate) async fn auto_on(app: &App) -> bool {
    let cfg = app.cfg.get().await;
    cfg.claude_api.enabled && cfg.claude_use_state(USE_CASE).is_some_and(|s| s.enabled && s.auto == Some(true))
}

/// 核心：查快取、沒有才打 API、寫快取。回 `{translation, model, cached, usage?}`。
/// 簡體中文（[`crate::zh_convert::local_applies`]）不打 API：本地 OpenCC 轉成台灣繁體，寫同一份快取（model `opencc-s2twp`、費用 0）。
/// 同一個快取鍵同時只有一個上游呼叫（single-flight）：預先翻跟網頁的手動／自動翻撞在一起，後到的等先到的，拿到快取，不重複扣費。
pub(crate) async fn translate(app: &App, id: &str, content: &str) -> Result<Value, LcError> {
    let path = cache_path(&app.data_dir, &cache_key(id, content));
    if let Some(hit) = load(&path) {
        return Ok(json!({"translation": hit.translation, "model": hit.model, "cached": true, "cost_usd": hit.cost_usd}));
    }
    if crate::zh_convert::local_applies(content) {
        return Ok(local_convert(content, &path));
    }
    let slot = flight_slot(&path);
    let guard = slot.lock().await;
    // 等鎖期間別人可能已經寫好快取：拿到控制權後重查，命中就不打 API。
    if let Some(hit) = load(&path) {
        drop(guard);
        flight_forget(&path, slot);
        return Ok(json!({"translation": hit.translation, "model": hit.model, "cached": true, "cost_usd": hit.cost_usd}));
    }
    let out = translate_upstream(app, id, content, &path).await;
    drop(guard);
    flight_forget(&path, slot);
    out
}

/// 簡體 → 台灣繁體（本地，不呼叫 API）。結果寫進同一份快取，之後命中即 `cached:true`。
fn local_convert(content: &str, path: &Path) -> Value {
    let translation = crate::zh_convert::to_traditional(content);
    let entry = CacheEntry { translation: translation.clone(), model: crate::zh_convert::MODEL.into(), prompt_version: PROMPT_VERSION.into(), cost_usd: Some(0.0), jev_prob: None };
    if let Err(e) = save(path, &entry) {
        tracing::warn!("message translate cache write failed: {e}");
    }
    json!({"translation": translation, "model": crate::zh_convert::MODEL, "cached": false, "cost_usd": 0.0})
}

const JEV_CHARS: usize = 6_000;
const JEV_QUESTION: &str = "is_faithful";
const JEV_PROMPT: &str = "Is the TRANSLATION a faithful Traditional Chinese (Taiwan) translation of the SOURCE, in Traditional Chinese, with nothing missing or added?";

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

async fn translate_upstream(app: &App, id: &str, content: &str, path: &Path) -> Result<Value, LcError> {
    let mut reply = claude_api::complete(app, build_request(content)).await.map_err(error_of)?;
    let mut translation = clean_reply(&reply.text);

    let min_prob = app.cfg.get().await.judge.translate_min_probability;
    let mut jev_prob = None;

    let mut rejected = local_rejected(content, &translation);
    if !rejected {
        let p = check_jev(app, id, content, &translation).await;
        if p.is_some_and(|v| v < min_prob) {
            rejected = true;
        } else {
            jev_prob = p;
        }
    }

    if rejected {
        reply = claude_api::complete(app, build_request(content)).await.map_err(error_of)?;
        translation = clean_reply(&reply.text);

        if is_mostly_english(content) && is_mostly_english(&translation) {
            return Err(LcError::Upstream("translate_untranslated".into()));
        }
        if !has_japanese_kana(content) && has_japanese_kana(&translation) {
            return Err(LcError::Upstream("translate_rejected".into()));
        }

        let p = check_jev(app, id, content, &translation).await;
        if p.is_some_and(|v| v < min_prob) {
            return Err(LcError::Upstream("translate_rejected".into()));
        }
        jev_prob = p;
    }

    let cost = reply.usage.as_ref().and_then(|u| cost_usd(&reply.model, u));
    let entry = CacheEntry {
        translation,
        model: reply.model.clone(),
        prompt_version: PROMPT_VERSION.into(),
        cost_usd: cost,
        jev_prob,
    };
    if let Err(e) = save(path, &entry) {
        tracing::warn!("message translate cache write failed: {e}");
    }
    let mut body = json!({"translation": entry.translation, "model": entry.model, "cached": false, "cost_usd": cost});
    if let Some(u) = &reply.usage {
        body["usage"] = usage_json(u);
    }
    Ok(body)
}

/// `POST /api/messages/{id}/translate`：只限 assistant 訊息。
pub async fn post_translate(
    State(app): State<Arc<App>>,
    AxPath(id): AxPath<String>,
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
    Ok(Json(translate(&app, &id, &content).await?))
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

/// 本地廉價檢查：日文假名（原文沒有但譯文有）或主要為英文（原文為英文且譯文未翻譯）。
pub(crate) fn local_rejected(source: &str, reply: &str) -> bool {
    (!has_japanese_kana(source) && has_japanese_kana(reply))
        || (is_mostly_english(source) && is_mostly_english(reply))
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
    let entry = CacheEntry { translation: out.clone(), model: reply.model, prompt_version: SUMMARY_VERSION.into(), cost_usd: cost, jev_prob: None };
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
        // Sonnet 5.5：2 + 10 + 2.5 + 0.2
        assert!((cost_usd("claude-sonnet-5-5", &u).unwrap() - 14.7).abs() < 1e-9);
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
        assert!(sys.contains("Traditional Chinese") && sys.contains("fenced code blocks") && sys.contains("commit hashes") && sys.contains("translation only"));
        assert!(r.prompt.contains("<source>\nRun `cargo test` in /tmp\n</source>"));
        assert!(r.prompt.contains("Traditional Chinese (zh-TW"));
        assert!(r.prompt.contains("請翻譯成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。"));
        assert!(sys.contains("請翻譯成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。"));
        assert!(SUMMARY_SYSTEM.contains("請寫成繁體中文（台灣用語，zh-TW）。不要用日文、簡體中文或英文。"));
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
    }

    impl MessagesClient for FakeSequence {
        fn complete<'a>(&'a self, _req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
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
        let f = Arc::new(FakeSequence { calls: AtomicUsize::new(0), replies: Mutex::new(replies) });
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
        assert!(matches!(&err, LcError::Upstream(msg) if msg == "translate_untranslated"), "錯誤訊息必須是 translate_untranslated: {err:?}");
        let key = cache_key("m_fail", content);
        assert!(!cache_path(&app.data_dir, &key).exists(), "失敗不寫入快取檔案");
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
    async fn jev_low_twice_returns_error_and_no_cache() {
        let (app, dir) = app(true).await;
        let (endpoint, jev_calls) = fake_jev_server(vec![0.2, 0.3]).await;
        enable_jev(&app, &dir, endpoint).await;

        let content = "The server returned 504 Gateway Timeout.";
        let replies = vec![
            "伺服器回傳了 504。".to_string(),
            "伺服器逾時。".to_string(),
        ];
        let f = fake_seq(&app, replies);
        let err = translate(&app, "m_jev_fail", content).await.unwrap_err();

        assert_eq!(f.calls.load(Ordering::SeqCst), 2, "Haiku 打兩次");
        assert_eq!(jev_calls.load(Ordering::SeqCst), 2, "Jev 問兩次");
        assert!(matches!(&err, LcError::Upstream(msg) if msg == "translate_rejected"), "兩次低機率回傳 translate_rejected: {err:?}");

        let key = cache_key("m_jev_fail", content);
        let path = cache_path(&app.data_dir, &key);
        assert!(!path.exists(), "Jev 兩次未通過不寫入快取");
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
        let call = |id: &str| post_translate(State(app.clone()), AxPath(id.into()), Extension(crate::api::RequestPrincipal::User));
        assert!(matches!(call("nope").await.unwrap_err(), LcError::NotFound(_)));
        let bot = Extension(crate::api::RequestPrincipal::Bot("x".into()));
        assert!(matches!(post_translate(State(app.clone()), AxPath("nope".into()), bot).await.unwrap_err(), LcError::Forbidden(_)));
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
