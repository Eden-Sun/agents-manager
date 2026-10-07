//! 單顆 bot 的 restart 合併：連點兩下（或兩個鈕、兩個分頁、腳本重送）不要真的重啟兩次。
//!
//! 一顆 bot 的 restart 是「停舊 run、起新 run」，在 bot 鎖裡一次做完；兩個請求本來就排隊，第二個等第一個做完就**再**重啟一次——
//! 剛起好的新 run 馬上又被收掉（c2 看到 `agent.start` 三次）。一鍵重啟（SPEC §6.9）同時只准一批；這裡是單顆版：
//! 同一顆 bot、同樣的重啟種類（`StartOpts` 一樣）已經有一個**仍在進行中**，後來的請求不重啟，等那一個做完、回同一個 `run_id`
//! （回應多一個 `coalesced:true`）。種類不同（例如 `?resume=fresh`）是另一件事，照做；前一個失敗的話，等著的那個自己重試。
//!
//! **只合併進行中的**：做完之後再來的請求一律是新的一次。以前做完 5 秒內也合併，結果改了 model／effort／設定之後馬上重啟，
//! 被當成「剛重啟過」就沒套到新設定（2026-10-02）；重啟要套用的是「現在」的設定，沒有一個安全的「剛做完」窗口。
//!
//! 只在 HTTP 入口合併：daemon 自己的流程（一鍵重啟、換版、輪替憑證…）直接呼叫 `restart_bot_with`，各自有各自的節流。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::watch;

#[derive(Clone, PartialEq, Eq)]
enum Outcome {
    Pending,
    Ok(String),
    Failed,
}

struct Slot {
    /// 哪一個領頭者的格子：後來的領頭者把它換掉之後，前一個收尾不能動新的。
    token: u64,
    key: String,
    rx: watch::Receiver<Outcome>,
}

fn slots() -> &'static Mutex<(u64, HashMap<String, Slot>)> {
    static S: OnceLock<Mutex<(u64, HashMap<String, Slot>)>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub enum Admission {
    /// 這個請求自己做；做完呼叫 [`Leader::finish`]，沒做成（回錯誤、被取消）就直接 drop。
    Lead(Leader),
    /// 已經有同樣的重啟在進行，等它做完了：這是它的 `run_id`。
    Joined(String),
}

pub struct Leader {
    bot_id: String,
    token: u64,
    tx: watch::Sender<Outcome>,
    finished: bool,
}

impl Leader {
    pub fn finish(mut self, run_id: &str) {
        self.finished = true;
        // 做完就把格子收掉：之後來的是新的一次。已經在等的請求手上有 receiver，照樣拿得到這個結果。
        if let Ok(mut g) = slots().lock() {
            if g.1.get(&self.bot_id).is_some_and(|s| s.token == self.token) {
                g.1.remove(&self.bot_id);
            }
        }
        let _ = self.tx.send(Outcome::Ok(run_id.to_string()));
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut g) = slots().lock() {
            if g.1.get(&self.bot_id).is_some_and(|s| s.token == self.token) {
                g.1.remove(&self.bot_id);
            }
        }
        let _ = self.tx.send(Outcome::Failed);
    }
}

/// `key`：重啟種類（`StartOpts` 的 Debug 字串），一樣才算同一件事。
pub async fn admit(bot_id: &str, key: &str) -> Admission {
    loop {
        let mut rx = {
            let mut g = slots().lock().unwrap_or_else(|e| e.into_inner());
            let waiting = match g.1.get(bot_id) {
                Some(slot) if slot.key == key => Some(slot.rx.clone()),
                _ => None,
            };
            match waiting {
                Some(rx) => rx,
                None => {
                    g.0 += 1;
                    let token = g.0;
                    let (tx, rx) = watch::channel(Outcome::Pending);
                    g.1.insert(bot_id.to_string(), Slot { token, key: key.to_string(), rx });
                    return Admission::Lead(Leader { bot_id: bot_id.to_string(), token, tx, finished: false });
                }
            }
        };
        // 等領頭的做完：成功就跟它同一個結果；它失敗了，回到迴圈自己當領頭（等於使用者的重試）。
        loop {
            match rx.borrow().clone() {
                Outcome::Ok(run_id) => return Admission::Joined(run_id),
                Outcome::Failed => break,
                Outcome::Pending => {}
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(tag: &str) -> String {
        format!("rc-{tag}-{}", crate::db::ulid())
    }

    #[tokio::test]
    async fn a_second_request_waits_for_the_first_and_shares_its_run() {
        let b = bot("share");
        let Admission::Lead(lead) = admit(&b, "k").await else { panic!("第一個自己做") };
        let b2 = b.clone();
        let waiter = tokio::spawn(async move { admit(&b2, "k").await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "領頭的還沒做完，後來的等著");
        lead.finish("run-1");
        match waiter.await.unwrap() {
            Admission::Joined(run) => assert_eq!(run, "run-1"),
            Admission::Lead(_) => panic!("不該再做一次"),
        }
    }

    /// 做完之後再來的請求是新的一次（改了設定馬上重啟要套用新設定，不能被當成「剛重啟過」）。
    #[tokio::test]
    async fn a_request_after_the_restart_finished_is_a_new_restart() {
        let b = bot("after");
        let Admission::Lead(lead) = admit(&b, "k").await else { panic!() };
        lead.finish("run-1");
        assert!(matches!(admit(&b, "k").await, Admission::Lead(_)), "已經做完：不合併");
    }

    #[tokio::test]
    async fn a_different_kind_of_restart_or_another_bot_is_not_coalesced() {
        let (a, other) = (bot("kind"), bot("other"));
        let Admission::Lead(l1) = admit(&a, "plain").await else { panic!() };
        assert!(matches!(admit(&a, "fresh").await, Admission::Lead(_)), "種類不同＝另一件事");
        assert!(matches!(admit(&other, "plain").await, Admission::Lead(_)), "別顆 bot 互不相干");
        l1.finish("run-1"); // 被換掉的格子：收尾不能動新領頭者的（這裡只是確認不會 panic、不會留下髒狀態）
    }

    #[tokio::test]
    async fn when_the_leader_fails_the_waiter_tries_for_itself() {
        let b = bot("fail");
        let Admission::Lead(lead) = admit(&b, "k").await else { panic!() };
        let b2 = b.clone();
        let waiter = tokio::spawn(async move { admit(&b2, "k").await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        drop(lead); // 沒做成（回錯誤或請求被取消）
        assert!(matches!(waiter.await.unwrap(), Admission::Lead(_)), "領頭的失敗：等著的自己重試，不假裝成功");
    }
}
