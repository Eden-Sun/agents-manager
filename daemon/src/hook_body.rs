use serde::{Deserialize, Serialize};
use serde_json::Value;

// `Serialize` 是給 `hook_inbox` 用的：整個 body 要原封不動存進收件匣再讀回來走 §6.7。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookBody {
    pub bot_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    /// 送出這則 hook 的 CLI 行程是替哪個 run 起的（pane env 的 `AM_RUN_ID`，issue #92）。
    /// 世代圍籬用它分辨「同一個 session、不同行程」：`--resume` 接回時新舊行程的 session id 一樣。
    /// 舊行程、遠端舊版 `hook.sh`、手寫的 body 沒有這一欄——那就照舊只看 session。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}
