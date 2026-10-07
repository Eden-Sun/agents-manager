//! Claude Code 2.1.281 的「Session paused」選單（畫面辨識在 [`crate::tui_prompts::is_session_paused_menu`]）。
//!
//! API 拒答或額度用完時 claude 停下來問「換模型重試／改 prompt 重試」（或「用額度續跑／換模型」），herdr 卻判成
//! `idle`：網頁不會彈出選項，回合還被終端備援收掉、把選單的一行存成回覆（2026-09-25 cf-ox-fork-fork）。這裡的原則：
//! * **一個鍵都不按**：換不換模型、要不要花額度是使用者的決定。
//! * **標成 blocked**：herdr 判 `idle`／`unknown` 時補標，網頁的 BlockedModal／BlockedPanel 才會彈出、用
//!   BlockedChoices 讓人自己點；備援與 stuck-turn 收尾看到 blocked 也就不收這個回合。
//! * **選單消失就還原**：照補標前的值還回去（CAS：這段期間 herdr 自己改過狀態就不動），叫醒排隊的 flush。
//!
//! 補標記在記憶體（`run_id` → 補標前的狀態）；daemon 重啟當下若是這裡補的 `blocked`，交給 herdr 下一次狀態事件或
//! reconcile 更正，巡邏看到選單還在會再補一次。
//!
//! 補標記是巡邏回頭看這個 run 的唯一理由（它已經不是 `idle`），所以**還原寫進 DB 之後才拿掉**（#565）：寫失敗就留著，
//! 下一輪巡邏重試；CAS 沒命中（herdr 或別的路徑已經改掉 `blocked`）算被取代，拿掉；run 不在或已結束也拿掉。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::db;

/// 補標記：補標前的狀態，加上最後一次補標的代號——還原寫回 DB 的那段期間選單又冒出來、重新補標的話，
/// 代號變了，還原就不能把新的補標記一起拿掉。
struct Forced {
    prev: String,
    epoch: u64,
    /// agy 的對話框（登入／條款／信任／權限…）的說明；claude 的 Session paused 是 `None`（`blocked_reason` 用固定的字）。
    label: Option<&'static str>,
}

fn forced() -> &'static Mutex<HashMap<String, Forced>> {
    static V: OnceLock<Mutex<HashMap<String, Forced>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

fn next_epoch() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn record_forced(run_id: &str, prev: String, label: Option<&'static str>) {
    let epoch = next_epoch();
    let mut m = forced().lock().unwrap();
    let f = m.entry(run_id.to_string()).or_insert(Forced { prev, epoch: 0, label });
    f.epoch = epoch;
    f.label = label;
}

pub fn get_forced(run_id: &str) -> Option<(String, u64)> {
    forced().lock().unwrap().get(run_id).map(|f| (f.prev.clone(), f.epoch))
}

/// 拿掉補標記——只在它還是 `epoch` 那一次補的時候。
pub fn retire(run_id: &str, epoch: u64) {
    let mut m = forced().lock().unwrap();
    if m.get(run_id).is_some_and(|f| f.epoch == epoch) {
        m.remove(run_id);
    }
}

/// 這個 run 現在是不是由這裡補標成 `blocked` 的（巡邏靠它把已經不是 idle 的 run 也看一眼，才還得回去）。
pub fn is_forced(run_id: &str) -> bool {
    forced().lock().unwrap().contains_key(run_id)
}

/// agy 對話框的說明（這個 run 是由這裡補標、而且卡在 agy 的對話框時）。
pub fn agy_label(run_id: &str) -> Option<&'static str> {
    forced().lock().unwrap().get(run_id).and_then(|f| f.label)
}

/// 巡邏收尾：補標記的 run 已經不在 active 名單上（結束或被刪）就拿掉，巡邏再也不會看它。
/// 以重讀 DB 為準；讀不到（DB 錯）就留著等下一輪。
pub async fn forget_ended(app: &impl crate::capabilities::Db, active: &[db::Run]) {
    let marked: Vec<(String, u64)> = forced()
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _)| !active.iter().any(|r| &r.id == *id))
        .map(|(id, f)| (id.clone(), f.epoch))
        .collect();
    for (id, epoch) in marked {
        match db::run(app.db(), &id).await {
            Ok(Some(r)) if matches!(r.state.as_str(), "starting" | "running" | "stopping") => {}
            Ok(_) => retire(&id, epoch),
            Err(_) => {}
        }
    }
}
