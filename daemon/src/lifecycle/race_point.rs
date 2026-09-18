//! 競態的注入點：**只在 `cargo test` 底下存在**（整個模組 `#[cfg(test)]`，呼叫端也包在 `#[cfg(test)]` 裡），
//! 正式二進位裡連呼叫都沒有。
//!
//! 有些窗口是「兩句寫入之間的那一瞬」：不拿 bot 鎖的另一條路（定時 sweeper、pane-exit 事件）剛好在那一瞬
//! 插進來。單執行緒測試插不進去，所以在那一瞬放一個具名的點：測試先 [`arm`] 一個一次性的動作，程式走到
//! [`hit`] 時就地跑它——等於讓「另一條路」確定性地落在窗口裡。沒掛的點什麼都不做。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

fn hooks() -> &'static Mutex<HashMap<(&'static str, String), Hook>> {
    static M: OnceLock<Mutex<HashMap<(&'static str, String), Hook>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 在 `point` 對 `key`（bot id／run id，平行的測試才不會互相踩到）掛一次性的動作。
pub(crate) fn arm<F, Fut>(point: &'static str, key: &str, f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let hook: Hook = Box::new(move || Box::pin(f()));
    hooks().lock().unwrap().insert((point, key.to_string()), hook);
}

/// 走到 `point`：有掛就跑一次（跑完就拿掉），沒掛什麼都不做。
pub(crate) async fn hit(point: &'static str, key: &str) {
    let hook = hooks().lock().unwrap().remove(&(point, key.to_string()));
    if let Some(h) = hook {
        h().await;
    }
}
