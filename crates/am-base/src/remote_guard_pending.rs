//! 遠端 bot「守門設定待重啟套用」的清單（使用者 2026-11-28）：連上主機時的守門熱更新寫入，`supervisor::incidents` 讀出來開 incident，
//! 所以 `agm health` 看得到。每台主機一份、整份替換；那次 run 已經不在跑的由讀的一方略過（下次刷新那台時自然換掉）。
//! 只存在記憶體裡：daemon 重啟後連上主機會用遠端 bot 目錄的 `guard.sig` 重新判斷一次。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 一顆守門設定待重啟套用的 bot（`run_id` 是判斷當下那次 run，重啟或停掉之後就不算）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRestart {
    pub bot_id: String,
    pub bot_name: String,
    pub kind: String,
    pub run_id: String,
}

fn table() -> &'static Mutex<HashMap<String, Vec<PendingRestart>>> {
    static T: OnceLock<Mutex<HashMap<String, Vec<PendingRestart>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// 換掉這台主機的整份清單（空的＝這台沒有待重啟的）。
pub fn set_host(host: &str, pending: Vec<PendingRestart>) {
    let mut t = table().lock().unwrap();
    if pending.is_empty() {
        t.remove(host);
    } else {
        t.insert(host.to_string(), pending);
    }
}

/// `(host, bot)`，依主機名排序。
pub fn snapshot() -> Vec<(String, PendingRestart)> {
    let t = table().lock().unwrap();
    let mut out: Vec<_> = t.iter().flat_map(|(h, v)| v.iter().map(move |p| (h.clone(), p.clone()))).collect();
    out.sort_by(|a, b| (&a.0, &a.1.bot_id).cmp(&(&b.0, &b.1.bot_id)));
    out
}
