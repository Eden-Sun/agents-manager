//! 分享 bot 的表：`shared_bots`（哪些 bot 是分享用 bot、哪一種、它的工作目錄）、`bot_shares`（開著的分享連結）與
//! `share_reply_visible`（擁有者送的哪幾則，bot 的回覆仍要給分享頁看）。
//!
//! 分享用 bot 有兩種（SPEC §20.1）：`restricted`（受限：套 `share::cage` 的籠子）與 `trusted`（信任分享，使用者 2026-10-04：
//! 權限就是一般 bot，只分享給絕對信任的人）。凡是「是不是分享 bot」的判斷（AGM 不清、outbox 不過期、停掉自動 resume、
//! 分享入口）兩種都算，用 [`is_share_bot`]；只有「要不要關進籠子」看 [`caged_workspace`]。
//!
//! token 是 capability：32 bytes 亂數（base64url，43 字）。DB 存 SHA-256（hex，入口查表＋常數時間比對用）、末 4 碼提示
//! 與原文（`token`，讓管理端隨時拿得回完整連結；跟 `bots.hook_token` 同一個 0600 DB、同一等級）。重產＝換掉 hash 與原文，
//! 舊連結當下失效；關掉＝刪掉那一列。加 `token` 欄之前開的分享只有 hash：入口照樣認，管理端拿不回網址（`token` NULL），重產一次就有。

use std::collections::HashSet;

use anyhow::Result;
use base64::Engine as _;
use rand::RngCore as _;
use sha2::Digest as _;
use sqlx::SqlitePool;

use crate::db;

pub const PROFILE_RESTRICTED: &str = "restricted";
pub const PROFILE_TRUSTED: &str = "trusted";

const SHARED_BOTS_SQL: &str = "CREATE TABLE IF NOT EXISTS shared_bots (
           bot_id TEXT PRIMARY KEY,
           -- restricted＝受限（籠子）、trusted＝信任分享（一般 bot 的權限）。建 bot 時決定，之後不能切換（要分享就新建一顆）。
           profile TEXT NOT NULL CHECK (profile IN ('restricted','trusted')),
           -- 它的 cwd：受限的是檔案工具唯一碰得到的地方（另加自己的 outbox）；兩種都把上傳的檔放在這裡的 `inbox/`。
           workspace TEXT NOT NULL,
           created_at TEXT NOT NULL
         )";
/// token 原文的長度（32 bytes → base64url 無 padding）。
pub const TOKEN_LEN: usize = 43;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    // 只收 'restricted' 的舊表：SQLite 改不了 CHECK，換名、照新定義建、搬資料、刪舊表（同一個交易）。
    // 先換名再建，新表的定義才跟全新 DB 一字不差（schema_guard 比的是它）。
    let old_sql: Option<String> = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'shared_bots'").fetch_optional(pool).await?;
    if old_sql.is_some_and(|sql| !sql.contains("'trusted'")) {
        let mut tx = db::begin_write(pool).await?;
        sqlx::query("ALTER TABLE shared_bots RENAME TO shared_bots_v1").execute(&mut *tx).await?;
        sqlx::query(SHARED_BOTS_SQL).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO shared_bots (bot_id, profile, workspace, created_at) SELECT bot_id, profile, workspace, created_at FROM shared_bots_v1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE shared_bots_v1").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    sqlx::query(SHARED_BOTS_SQL).execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_shares (
           bot_id TEXT PRIMARY KEY,
           -- SHA-256(token) 的 hex：入口用它查表。
           token_hash TEXT NOT NULL UNIQUE,
           token_hint TEXT NOT NULL,
           created_at TEXT NOT NULL,
           rotated_at TEXT,
           last_used_at TEXT,
           -- token 原文，管理端組完整網址用。NULL＝加這欄之前開的（只有 hash），重產一次就有。
           token TEXT
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS share_reply_visible (
           -- 擁有者（或別顆 bot）送給分享 bot 的 user 訊息：預設那一回合 bot 的回覆也不給分享頁看，
           -- 送的時候帶 `share_reply_visible:true` 才記在這裡（API.md §5.6）。
           message_id TEXT PRIMARY KEY,
           created_at TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    let has_token: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_table_info('bot_shares') WHERE name = 'token')").fetch_one(pool).await?;
    if !has_token {
        sqlx::query("ALTER TABLE bot_shares ADD COLUMN token TEXT").execute(pool).await?;
    }
    let has_allow_embed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pragma_table_info('bot_shares') WHERE name = 'allow_embed')").fetch_one(pool).await?;
    if !has_allow_embed {
        sqlx::query("ALTER TABLE bot_shares ADD COLUMN allow_embed INTEGER NOT NULL DEFAULT 0").execute(pool).await?;
    }
    Ok(())
}

#[allow(unused_imports)]
pub use crate::db::{caged_workspace, is_caged, is_share_bot, share_workspace as workspace};

/// 同 [`caged_workspace`]：受限分享 bot 的工作目錄（issue #828）。
pub async fn restricted_workspace(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    caged_workspace(pool, bot_id).await
}

/// 每顆分享用 bot 的種類（`restricted`／`trusted`）；投影給主 UI 的 `share_profile`。
pub async fn profiles(pool: &SqlitePool) -> Result<std::collections::HashMap<String, String>, sqlx::Error> {
    Ok(sqlx::query_as::<_, (String, String)>("SELECT bot_id, profile FROM shared_bots").fetch_all(pool).await?.into_iter().collect())
}

/// 擁有者這一則送給分享 bot 的訊息：這一回合 bot 的回覆照樣給分享頁看。
pub async fn mark_reply_visible(pool: &SqlitePool, message_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO share_reply_visible (message_id, created_at) VALUES (?,?)").bind(message_id).bind(db::now()).execute(pool).await.map(|_| ())
}

/// 分享開著的 bot（`bot_shares` 有一列）；側欄的 🔗 亮不亮用。
pub async fn shared_ids(pool: &SqlitePool) -> Result<HashSet<String>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, String>("SELECT bot_id FROM bot_shares").fetch_all(pool).await?.into_iter().collect())
}

#[cfg(test)]
pub async fn insert_restricted(pool: &SqlitePool, bot_id: &str, workspace: &str) -> Result<(), sqlx::Error> {
    insert_share_bot(pool, bot_id, PROFILE_RESTRICTED, workspace).await
}

/// 建 bot **之前**先記下來：config 一寫進去、投影出那一列之後，任何一次啟動都已經是受限的，沒有「先當一般 bot 起來」的空窗。
/// `profile` 是 [`PROFILE_RESTRICTED`] 或 [`PROFILE_TRUSTED`]。
pub async fn insert_share_bot(pool: &SqlitePool, bot_id: &str, profile: &str, workspace: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO shared_bots (bot_id, profile, workspace, created_at) VALUES (?,?,?,?)")
        .bind(bot_id)
        .bind(profile)
        .bind(workspace)
        .bind(db::now())
        .execute(pool)
        .await
        .map(|_| ())
}

/// 建 bot 失敗時收回 [`insert_share_bot`]。
pub async fn delete_restricted(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM shared_bots WHERE bot_id = ?").bind(bot_id).execute(pool).await?;
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShareRow {
    pub token_hint: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// NULL＝舊資料（只有 hash），拿不回完整網址。
    pub token: Option<String>,
    /// 信任分享才能開：這條連結允許被別的網站用 `<iframe>` 嵌進去（SPEC §20.1a）。
    pub allow_embed: bool,
}

pub async fn share(pool: &SqlitePool, bot_id: &str) -> Result<Option<ShareRow>, sqlx::Error> {
    sqlx::query_as("SELECT token_hint, created_at, last_used_at, token, allow_embed FROM bot_shares WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await
}

/// 這顆 bot 是不是信任分享（`allow_embed` 只給它）。
pub async fn is_trusted(pool: &SqlitePool, bot_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM shared_bots WHERE bot_id = ? AND profile = 'trusted')").bind(bot_id).fetch_one(pool).await
}

/// 設定「允許 iframe 嵌入」。分享沒開著＝不動（回 `false`）；是否信任分享由呼叫端先擋。
pub async fn set_allow_embed(pool: &SqlitePool, bot_id: &str, allow: bool) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("UPDATE bot_shares SET allow_embed = ? WHERE bot_id = ?").bind(allow).bind(bot_id).execute(pool).await?.rows_affected() == 1)
}

/// token 有效、分享還開著、是信任分享、而且勾了「允許 iframe 嵌入」才 `true`。token 不對、分享關了、受限 bot 一律 `false`
/// （同 [`resolve`] 的查法，再加上 `allow_embed`）。
pub async fn embed_allowed(pool: &SqlitePool, token: &str) -> Result<bool, sqlx::Error> {
    if !token_shape_ok(token) {
        return Ok(false);
    }
    let want = token_hash(token);
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT s.token_hash, s.allow_embed FROM bot_shares s
           JOIN shared_bots r ON r.bot_id = s.bot_id AND r.profile = 'trusted'
           JOIN bots b ON b.id = s.bot_id AND b.deleted_at IS NULL
           JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL
          WHERE s.token_hash = ?",
    )
    .bind(&want)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some_and(|(have, allow)| allow && crate::agent_relay::ct_eq(&have, &want)))
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn token_hash(token: &str) -> String {
    sha2::Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 只認我們發得出來的形狀：43 字 base64url。其他一律不查 DB。
pub fn token_shape_ok(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn hint(token: &str) -> String {
    format!("…{}", &token[token.len().saturating_sub(4)..])
}

/// 開分享。已經開著就不動（回 `None`，網址照 [`share`] 存的拿）：重按開關不該讓已經發出去的連結失效，要換請走 [`rotate`]。
pub async fn enable(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    let token = new_token();
    let done = sqlx::query(
        "INSERT OR IGNORE INTO bot_shares (bot_id, token_hash, token_hint, created_at, token)
         SELECT ?,?,?,?,? WHERE EXISTS (
           SELECT 1 FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
             JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL WHERE r.bot_id = ?
         )",
    )
        .bind(bot_id)
        .bind(token_hash(&token))
        .bind(hint(&token))
        .bind(db::now())
        .bind(&token)
        .bind(bot_id)
        .execute(pool)
        .await?;
    Ok((done.rows_affected() == 1).then_some(token))
}

/// 換新 token：舊的 hash 與原文一起被蓋掉，同一刻起舊連結 404。沒開著＝`None`。
pub async fn rotate(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    let token = new_token();
    let done = sqlx::query(
        "UPDATE bot_shares SET token_hash = ?, token_hint = ?, token = ?, rotated_at = ? WHERE bot_id = ? AND EXISTS (
           SELECT 1 FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
             JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL WHERE r.bot_id = bot_shares.bot_id
         )",
    )
        .bind(token_hash(&token))
        .bind(hint(&token))
        .bind(&token)
        .bind(db::now())
        .bind(bot_id)
        .execute(pool)
        .await?;
    Ok((done.rows_affected() == 1).then_some(token))
}

pub async fn disable(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await.map(|_| ())
}

/// token → bot id。分享關了、bot 刪了、不是受限 bot（不該發生，但不信任單一張表）一律 `None`。
/// 用 hash 查表，查到之後再常數時間比一次 hash（不靠 SQLite 的字串比較當最後一道）。
pub async fn resolve(pool: &SqlitePool, token: &str) -> Result<Option<String>, sqlx::Error> {
    if !token_shape_ok(token) {
        return Ok(None);
    }
    let want = token_hash(token);
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT s.bot_id, s.token_hash FROM bot_shares s
           JOIN shared_bots r ON r.bot_id = s.bot_id
           JOIN bots b ON b.id = s.bot_id AND b.deleted_at IS NULL
           JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL
          WHERE s.token_hash = ?",
    )
    .bind(&want)
    .fetch_optional(pool)
    .await?;
    Ok(row.filter(|(_, have)| crate::agent_relay::ct_eq(have, &want)).map(|(id, _)| id))
}

/// 記「最後一次有人用」，一分鐘最多寫一次。
pub async fn touch(pool: &SqlitePool, bot_id: &str) {
    let _ = sqlx::query("UPDATE bot_shares SET last_used_at = ? WHERE bot_id = ? AND (last_used_at IS NULL OR last_used_at < ?)")
        .bind(db::now())
        .bind(bot_id)
        .bind(db::iso_in(-60))
        .execute(pool)
        .await;
}

/// Permanently revoke the current public capability while retaining the restricted-bot profile.
pub async fn revoke_bot(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await.map(|_| ())
}

pub async fn project_share_ids(pool: &SqlitePool, project_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT s.bot_id FROM bot_shares s JOIN bots b ON b.id = s.bot_id WHERE b.project_id = ?",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

pub async fn revoke_project(pool: &SqlitePool, project_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id IN (SELECT id FROM bots WHERE project_id = ?)")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}
