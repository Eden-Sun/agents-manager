//! 分享 bot 給 end user（SPEC「分享 bot」，使用者 2026-10-03）。
//!
//! 拿到連結的人＝網路上任何人，他能對一顆跑在這台機器上的 agent 下指令。所以分成三層，各自有測試釘住：
//!
//! - [`cage`]：只有建 bot 時就選「分享用（受限）」的 bot（`shared_bots` 有一列）能分享。它的 claude 用
//!   `--restricted`（沒有 Bash／WebFetch、檔案工具只在工作目錄）＋`dontAsk`（沒預先允許的一律拒絕，不會停在權限框等人），
//!   工作目錄是 `<data_dir>/shared-bots/<bot_id>/workspace/`，pane env 只留 hook 要的幾個，帳號目錄明確 deny。
//!   它的 hook token 只能打 `/hook/*`：`/api`、`/relay/*`、`/build-slots/*` 一律拒絕（[`refuses_bot_principal`]）。
//! - [`admin`]：主 API（7788，只收 UI token）上開／關／重產連結。DB 只存 token 的 SHA-256。
//! - [`portal`]：獨立 listener（`[share] listen`），router 上只有 `/s/{token}/…` 與分享頁的靜態檔，沒有 fallback 到主 API。

pub(crate) mod admin;
pub(crate) mod cage;
pub(crate) mod multipart;
pub(crate) mod portal;
pub(crate) mod store;

#[cfg(test)]
mod tests;

/// 分享頁 end user 送進來的訊息記在 `messages.relay_from` 的哨符（跟 `daemon` 同一類：不是 bot id）。
/// 輸出時 `source` 改報 `share`（`db::Message` 的序列化），UI 標「🔗 分享使用者」。
pub(crate) const SHARE_SENDER: &str = "share";

/// 受限 bot 的 hook token 只用來打自己的 hook：其他任何拿 bot 身分進來的路一律擋。讀不到 DB 也擋（fail closed）。
pub(crate) async fn refuses_bot_principal(db: &sqlx::SqlitePool, bot_id: &str) -> bool {
    !matches!(store::is_restricted(db, bot_id).await, Ok(false))
}
