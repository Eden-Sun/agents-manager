//! 持久化記號的純 SQL／字串規則：保溫回合（`client_request_id` 前綴）與熱壓系統訊息（內容前綴）。
//!
//! 這些前綴本身就是寫進 DB 的資料，所以認它們的規則住在 db 這一側（`db::migrate` 的 `messages.keep_warm` trigger／回填也要用）；
//! `cache_clock` 只是用它們算時間（並 re-export 這裡的名字，既有路徑不變）。

/// 保溫回合的 `client_request_id` 前綴（後面接錨點時間，見 `primary_keep_warm`）。不加欄位：這個前綴本身就持久、跨重啟認得出來。
pub const KEEP_WARM_CRID_PREFIX: &str = "keep-warm:";
/// 改名前（`keepalive:`）寫進 DB 的保溫回合；舊資料一律照樣認得（cache_clock、未讀、`messages.keep_warm` 標記）。
pub const LEGACY_KEEP_WARM_CRID_PREFIX: &str = "keepalive:";
/// 熱壓在聊天室留的系統訊息開頭，同時是「這個錨點之後熱壓過了」的持久記號。
pub const WARM_COMPACT_NOTE_PREFIX: &str = "主力熱壓：";
/// 改名前的熱壓訊息開頭；DB 裡的舊訊息照樣是記號。
pub const LEGACY_WARM_COMPACT_NOTE_PREFIX: &str = "主力 cache 到點壓縮：";

/// 這個 `client_request_id` 是不是保溫回合的（新舊前綴都算）。
pub fn is_keep_warm_crid(crid: &str) -> bool {
    crid.starts_with(KEEP_WARM_CRID_PREFIX) || crid.starts_with(LEGACY_KEEP_WARM_CRID_PREFIX)
}

/// SQL 條件：`col` 是保溫回合的 `client_request_id`（NULL 是 NULL，不是 true）。新舊前綴都收。
pub fn keep_warm_crid_sql(col: &str) -> String {
    format!("({col} LIKE '{KEEP_WARM_CRID_PREFIX}%' OR {col} LIKE '{LEGACY_KEEP_WARM_CRID_PREFIX}%')")
}

/// SQL 條件：`col` 是熱壓留下的系統訊息內容。
pub fn warm_compact_note_sql(col: &str) -> String {
    format!("({col} LIKE '{WARM_COMPACT_NOTE_PREFIX}%' OR {col} LIKE '{LEGACY_WARM_COMPACT_NOTE_PREFIX}%')")
}
