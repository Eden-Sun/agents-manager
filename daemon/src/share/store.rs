//! 分享 bot 的兩張表：`shared_bots`（哪些 bot 是受限的分享用 bot、它的工作目錄）與 `bot_shares`（開著的分享連結）。
//!
//! token 是 capability：32 bytes 亂數（base64url，43 字），DB 只存 SHA-256（hex）與末 4 碼提示；完整 token 只在
//! 剛開／重產的那一個回應裡出現一次。重產＝換掉 hash，舊連結當下失效；關掉＝刪掉那一列。

use std::collections::HashSet;

use anyhow::Result;
use base64::Engine as _;
use rand::RngCore as _;
use sha2::Digest as _;
use sqlx::SqlitePool;

use crate::db;

pub(crate) const PROFILE_RESTRICTED: &str = "restricted";
/// token 原文的長度（32 bytes → base64url 無 padding）。
pub(crate) const TOKEN_LEN: usize = 43;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS shared_bots (
           bot_id TEXT PRIMARY KEY,
           -- 目前只有一種：分享用的受限 bot。建 bot 時決定，之後不能切換（要分享就新建一顆）。
           profile TEXT NOT NULL CHECK (profile IN ('restricted')),
           -- `<data_dir>/shared-bots/<bot_id>/workspace`：它的 cwd，也是檔案工具唯一碰得到的地方（另加自己的 outbox）。
           workspace TEXT NOT NULL,
           created_at TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_shares (
           bot_id TEXT PRIMARY KEY,
           -- SHA-256(token) 的 hex；原文不落地。
           token_hash TEXT NOT NULL UNIQUE,
           token_hint TEXT NOT NULL,
           created_at TEXT NOT NULL,
           rotated_at TEXT,
           last_used_at TEXT
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn is_restricted(pool: &SqlitePool, bot_id: &str) -> Result<bool, sqlx::Error> {
    Ok(workspace(pool, bot_id).await?.is_some())
}

/// 受限 bot 的工作目錄；不是受限 bot＝`None`。
pub(crate) async fn workspace(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT workspace FROM shared_bots WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await
}

pub(crate) async fn restricted_ids(pool: &SqlitePool) -> Result<HashSet<String>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, String>("SELECT bot_id FROM shared_bots").fetch_all(pool).await?.into_iter().collect())
}

/// 建 bot **之前**先記下來：config 一寫進去、投影出那一列之後，任何一次啟動都已經是受限的，沒有「先當一般 bot 起來」的空窗。
pub(crate) async fn insert_restricted(pool: &SqlitePool, bot_id: &str, workspace: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO shared_bots (bot_id, profile, workspace, created_at) VALUES (?,?,?,?)")
        .bind(bot_id)
        .bind(PROFILE_RESTRICTED)
        .bind(workspace)
        .bind(db::now())
        .execute(pool)
        .await
        .map(|_| ())
}

/// 建 bot 失敗時收回 [`insert_restricted`]。
pub(crate) async fn delete_restricted(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM shared_bots WHERE bot_id = ?").bind(bot_id).execute(pool).await?;
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ShareRow {
    pub token_hint: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

pub(crate) async fn share(pool: &SqlitePool, bot_id: &str) -> Result<Option<ShareRow>, sqlx::Error> {
    sqlx::query_as("SELECT token_hint, created_at, last_used_at FROM bot_shares WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await
}

pub(crate) fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn token_hash(token: &str) -> String {
    sha2::Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 只認我們發得出來的形狀：43 字 base64url。其他一律不查 DB。
pub(crate) fn token_shape_ok(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn hint(token: &str) -> String {
    format!("…{}", &token[token.len().saturating_sub(4)..])
}

/// 開分享。已經開著就不動（回 `None`）：重按開關不該讓已經發出去的連結失效，要換請走 [`rotate`]。
pub(crate) async fn enable(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    let token = new_token();
    let done = sqlx::query(
        "INSERT OR IGNORE INTO bot_shares (bot_id, token_hash, token_hint, created_at)
         SELECT ?,?,?,? WHERE EXISTS (
           SELECT 1 FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
             JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL WHERE r.bot_id = ?
         )",
    )
        .bind(bot_id)
        .bind(token_hash(&token))
        .bind(hint(&token))
        .bind(db::now())
        .bind(bot_id)
        .execute(pool)
        .await?;
    Ok((done.rows_affected() == 1).then_some(token))
}

/// 換新 token：舊的 hash 被蓋掉，同一刻起舊連結 404。沒開著＝`None`。
pub(crate) async fn rotate(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>, sqlx::Error> {
    let token = new_token();
    let done = sqlx::query(
        "UPDATE bot_shares SET token_hash = ?, token_hint = ?, rotated_at = ? WHERE bot_id = ? AND EXISTS (
           SELECT 1 FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
             JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL WHERE r.bot_id = bot_shares.bot_id
         )",
    )
        .bind(token_hash(&token))
        .bind(hint(&token))
        .bind(db::now())
        .bind(bot_id)
        .execute(pool)
        .await?;
    Ok((done.rows_affected() == 1).then_some(token))
}

pub(crate) async fn disable(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await.map(|_| ())
}

/// token → bot id。分享關了、bot 刪了、不是受限 bot（不該發生，但不信任單一張表）一律 `None`。
/// 用 hash 查表，查到之後再常數時間比一次 hash（不靠 SQLite 的字串比較當最後一道）。
pub(crate) async fn resolve(pool: &SqlitePool, token: &str) -> Result<Option<String>, sqlx::Error> {
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
    Ok(row.filter(|(_, have)| crate::api::ct_eq(have, &want)).map(|(id, _)| id))
}

/// 記「最後一次有人用」，一分鐘最多寫一次。
pub(crate) async fn touch(pool: &SqlitePool, bot_id: &str) {
    let _ = sqlx::query("UPDATE bot_shares SET last_used_at = ? WHERE bot_id = ? AND (last_used_at IS NULL OR last_used_at < ?)")
        .bind(db::now())
        .bind(bot_id)
        .bind(db::iso_in(-60))
        .execute(pool)
        .await;
}

/// Permanently revoke the current public capability while retaining the restricted-bot profile.
pub(crate) async fn revoke_bot(pool: &SqlitePool, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id = ?").bind(bot_id).execute(pool).await.map(|_| ())
}

pub(crate) async fn project_share_ids(pool: &SqlitePool, project_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT s.bot_id FROM bot_shares s JOIN bots b ON b.id = s.bot_id WHERE b.project_id = ?",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

pub(crate) async fn revoke_project(pool: &SqlitePool, project_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM bot_shares WHERE bot_id IN (SELECT id FROM bots WHERE project_id = ?)")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}
