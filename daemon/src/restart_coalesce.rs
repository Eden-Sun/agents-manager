//! 單顆 bot 的 restart 合併：連點兩下（或兩個鈕、兩個分頁、腳本重送）不要真的重啟兩次。
//!
//! 一顆 bot 的 restart 是「停舊 run、起新 run」，在 bot 鎖裡一次做完；兩個請求本來就排隊，第二個等第一個做完就**再**重啟一次——
//! 剛起好的新 run 馬上又被收掉（c2 看到 `agent.start` 三次）。一鍵重啟（SPEC §6.9）同時只准一批；這裡是單顆版：
//! 同一顆 bot、同樣的重啟種類（`StartOpts` 一樣）已經有一個在進行、或 [`WINDOW`] 內剛做完，後來的請求不重啟，等同一個結果、回同一個 `run_id`
//! （回應多一個 `coalesced:true`）。種類不同（例如 `?resume=fresh`）是另一件事，照做；前一個失敗的話，等著的那個自己重試。
//!
//! 只在 HTTP 入口合併：daemon 自己的流程（一鍵重啟、換版、輪替憑證…）直接呼叫 `restart_bot_with`，各自有各自的節流。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// 剛做完多久之內，同樣的重啟請求還算「同一次」。
pub const WINDOW: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq, Eq)]
enum Outcome {
    Pending,
    Ok(String),
    Failed,
}

enum State {
    Running(watch::Receiver<Outcome>),
    Done { at: Instant, run_id: String },
}

struct Slot {
    /// 哪一個領頭者的格子：後來的領頭者把它換掉之後，前一個收尾不能動新的。
    token: u64,
    key: String,
    state: State,
}

fn slots() -> &'static Mutex<(u64, HashMap<String, Slot>)> {
    static S: OnceLock<Mutex<(u64, HashMap<String, Slot>)>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub enum Admission {
    /// 這個請求自己做；做完呼叫 [`Leader::finish`]，沒做成（回錯誤、被取消）就直接 drop。
    Lead(Leader),
    /// 已經有同樣的重啟在進行或剛做完：這是它的 `run_id`。
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
        if let Ok(mut g) = slots().lock() {
            if let Some(slot) = g.1.get_mut(&self.bot_id).filter(|s| s.token == self.token) {
                slot.state = State::Done { at: Instant::now(), run_id: run_id.to_string() };
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
    admit_within(bot_id, key, WINDOW).await
}

pub(crate) async fn admit_within(bot_id: &str, key: &str, window: Duration) -> Admission {
    loop {
        let mut rx = {
            let mut g = slots().lock().unwrap_or_else(|e| e.into_inner());
            let waiting = match g.1.get(bot_id) {
                Some(slot) if slot.key == key => match &slot.state {
                    State::Done { at, run_id } if at.elapsed() < window => return Admission::Joined(run_id.clone()),
                    State::Done { .. } => None,
                    State::Running(rx) => Some(rx.clone()),
                },
                _ => None,
            };
            match waiting {
                Some(rx) => rx,
                None => {
                    g.0 += 1;
                    let token = g.0;
                    let (tx, rx) = watch::channel(Outcome::Pending);
                    g.1.insert(bot_id.to_string(), Slot { token, key: key.to_string(), state: State::Running(rx) });
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "領頭的還沒做完，後來的等著");
        lead.finish("run-1");
        match waiter.await.unwrap() {
            Admission::Joined(run) => assert_eq!(run, "run-1"),
            Admission::Lead(_) => panic!("不該再做一次"),
        }
    }

    #[tokio::test]
    async fn just_finished_still_counts_but_only_within_the_window() {
        let b = bot("window");
        let Admission::Lead(lead) = admit_within(&b, "k", Duration::from_millis(80)).await else { panic!() };
        lead.finish("run-1");
        assert!(matches!(admit_within(&b, "k", Duration::from_millis(80)).await, Admission::Joined(r) if r == "run-1"));
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(matches!(admit_within(&b, "k", Duration::from_millis(80)).await, Admission::Lead(_)), "窗口過了：新的一次");
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(lead); // 沒做成（回錯誤或請求被取消）
        assert!(matches!(waiter.await.unwrap(), Admission::Lead(_)), "領頭的失敗：等著的自己重試，不假裝成功");
    }
}
