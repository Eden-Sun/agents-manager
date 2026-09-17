//! Issue #77 探索用的原型：在 `1a2aa019`／`77e64b35`／`12208eda` 把 lifecycle 收斂成「宣告式
//! 轉移表 + DB trigger／CAS」之後，per-bot actor runtime 值不值得做。
//!
//! **整個檔案只在 `cargo test` 底下編**（`lifecycle/mod.rs` 用 `#[cfg(test)] mod
//! actor_runtime_eval_prototype;` 掛進來，跟 issue #81 的 `native_transport_prototype.rs`
//! 同一個做法），不會出現在 `cargo build`／正式二進位裡。完整分析、逐題回答見
//! `docs/ACTOR-RUNTIME-EVAL.md`；這裡只放能跑、能轉紅、把敘述釘進程式碼的那一小塊。
//!
//! 這裡驗證的是**一個 mailbox-based actor 有沒有「收下訊息」與「訊息造成的效果撐過重啟」
//! 之間的空窗**——不是真的搭一套生產用的 actor runtime（那正是這個 issue 要先問清楚值不值得
//! 做的東西）。

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// 極簡的 per-bot actor：一個 channel + 一顆 task。`accepted` 代表訊息已經進了 actor 的手裡
/// （mailbox 收下、開始處理）；`persisted` 代表這個模型裡唯一撐得過「行程重啟」的東西
/// （真實系統裡就是 SQLite 那一列）。兩者之間刻意留一段 `sleep` 模擬「正在打字／正在等 herdr
/// 回應」那段時間——訊息確實被 actor 拿在手上，但還沒有任何持久化的痕跡。
struct ToyBotActor {
    tx: mpsc::Sender<String>,
    task: tokio::task::JoinHandle<()>,
}

impl ToyBotActor {
    fn spawn(persisted: Arc<Mutex<Vec<String>>>, accepted: Arc<Mutex<Vec<String>>>) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(8);
        let task = tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                accepted.lock().unwrap().push(cmd.clone());
                // 模擬「還在處理，還沒寫進 DB」的那段時間。
                tokio::time::sleep(Duration::from_millis(60)).await;
                persisted.lock().unwrap().push(cmd);
            }
        });
        Self { tx, task }
    }

    async fn send(&self, cmd: &str) {
        let _ = self.tx.send(cmd.to_string()).await;
    }

    /// 模擬「daemon 被砍掉」，不是「daemon 收工不再接新工作」——單純 `drop(self)` 只會關掉
    /// channel，task 手上那份 `persisted`／`accepted` 的 `Arc` 是它自己 move 進去的，跟 sender
    /// 的生死無關，仍然會把正在處理的那一則跑完再結束。真的重啟／崩潰要用 `abort()`：task
    /// 在下一個 `.await` 點就被砍斷，正在等的那個 `sleep` 永遠不會醒過來。
    fn kill(self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// actor 收下訊息（`accepted` 看得到）到真正落地（`persisted` 看得到，代表 SQLite 那一列）
    /// 之間有空窗；daemon 在空窗期被砍掉重啟（`kill()`／`abort()`，不是優雅收工——優雅收工會把
    /// 手上那則做完才停，那就不是在測「重啟」了）之後，這則訊息就是沒了——`persisted`（代表 DB）
    /// 看不到它，新開的 actor 也不知道曾經有這件事，除非另外有東西在「收下」的當下就先寫了 DB。
    /// **這正是 actor 為什麼不能取代現有『先寫 DB 再算數』的路徑，只能疊加在它前面**
    /// （見 docs/ACTOR-RUNTIME-EVAL.md 的「daemon restart」一節）。
    #[tokio::test]
    async fn a_message_accepted_but_not_yet_persisted_is_lost_when_the_actor_restarts() {
        let persisted = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let actor = ToyBotActor::spawn(persisted.clone(), accepted.clone());
        actor.send("type this prompt").await;

        // 等到訊息確定被 actor 收下（進了 mailbox），但還沒等它處理完（60ms 的模擬工作）。
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert_eq!(accepted.lock().unwrap().as_slice(), ["type this prompt"], "訊息已經被 actor 收下");
        assert!(persisted.lock().unwrap().is_empty(), "但還沒寫進『DB』——這就是那段空窗");

        actor.kill(); // 模擬 daemon 在這個空窗期被砍掉重啟：task 在下一個 await 點被腰斬。

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(persisted.lock().unwrap().is_empty(), "訊息真的沒了——新開的 actor 只能從 DB 重建狀態，DB 裡沒有這一筆");
    }

    /// 對照組：`turn_controller::set_status` 是現有模型的代表——一次 `UPDATE ... WHERE status=?`
    /// 就是「接受」這個動作本身，呼叫端拿到 `Outcome::Applied` 的那一刻，DB 已經是新狀態。
    /// 沒有「先進某個中繼結構、之後才落地」的兩段式，所以沒有上面那條測試示範的空窗。
    #[tokio::test]
    async fn the_current_model_has_no_such_window_because_the_db_write_is_the_acceptance() {
        let e = crate::testing::env().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('ar-bot',?,'ar-bot','claude','tok-ar',?)")
            .bind(&e.project_id)
            .bind(&now)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('ar-conv','ar-bot',?)").bind(&now).execute(&e.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('ar-turn','ar-conv','web','in_flight','ok',?)")
            .bind(&now)
            .execute(&e.app.db)
            .await
            .unwrap();

        let outcome = crate::lifecycle::turn_controller::set_status(&e.app.db, "ar-turn", "in_flight", "failed", "actor eval").await.unwrap();
        assert_eq!(outcome, crate::lifecycle::turn_controller::Outcome::Applied);
        // 呼叫回來的當下 DB 已經是新狀態，不像上面的 actor 原型還要再等一段處理時間才落地。
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id='ar-turn'").fetch_one(&e.app.db).await.unwrap();
        assert_eq!(status, "failed");
    }

    /// 把設計文件裡「11 個檔案呼叫 `bot_lock`、刪除是唯一會同時持多把的路徑」這兩個具體說法，
    /// 釘回真正的程式碼——不是人工數過一次就寫死在 .md 裡。這份清單改變（多了呼叫點、拿掉了
    /// `lock_bots_in_order` 那句排序說明），這條測試會轉紅，逼著 `docs/ACTOR-RUNTIME-EVAL.md`
    /// 跟著更新，不會悄悄跟程式碼的實際形狀脫鉤。
    #[test]
    fn the_bot_lock_ingress_claims_in_the_design_doc_still_match_the_code() {
        let sources: &[(&str, &str)] = &[
            ("api.rs", include_str!("../api.rs")),
            ("hookrecv.rs", include_str!("../hookrecv.rs")),
            ("default_session.rs", include_str!("../default_session.rs")),
            ("reconcile.rs", include_str!("../reconcile.rs")),
            ("lifecycle/poller.rs", include_str!("poller.rs")),
            ("lifecycle/prompt.rs", include_str!("prompt.rs")),
            ("lifecycle/queue.rs", include_str!("queue.rs")),
            ("lifecycle/screen.rs", include_str!("screen.rs")),
            ("lifecycle/slash.rs", include_str!("slash.rs")),
            ("lifecycle/start.rs", include_str!("start.rs")),
            ("lifecycle/stop.rs", include_str!("stop.rs")),
            ("lifecycle/stuck_turns.rs", include_str!("stuck_turns.rs")),
        ];
        for (name, src) in sources {
            assert!(src.contains("bot_lock("), "設計文件說 {name} 有呼叫 bot_lock，現在沒有了——評估的『ingress 分散在 N 個檔案』這個說法要重算");
        }
        assert!(
            include_str!("../api.rs").contains("依 id 排序、一次拿齊"),
            "lock_bots_in_order 排序說明的措辭變了——docs/ACTOR-RUNTIME-EVAL.md 的『actor 換不掉 lock ordering』一節引用的就是這句"
        );
    }
}
