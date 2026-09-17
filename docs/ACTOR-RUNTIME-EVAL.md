# Per-bot actor runtime：評估（issue #77）

**結論：不建議，現階段。** 今晚讓 issue #77 前提成立的三顆「lifecycle centralization」commit
（`1a2aa019` #68 Turn、`77e64b35` #71 Assignment、`12208eda` #74 Mission）解決的是「哪些狀態
轉移合法」這個問題，用的是**宣告式轉移表 + DB trigger／CAS**，不是 actor／mailbox；這個做法已經
用很小的改動把 issue #77 真正關心的幾類 race 堵掉了大半，而且是**走哪條路徑都繞不過去**的堵法
（trigger 裝在 DB 上，不是裝在某個 dispatcher 裡）。actor runtime 要解的問題，現在已經有更便宜、
風險更低的解法在生產路徑上跑著、被 1000 多條測試釘住；剩下沒解的部分（多 bot 操作的鎖排序、
50 個呼叫點分散在 12 個檔案），actor 換一種方式重現同樣的難題，不是拿掉它。

沒有做任何生產程式改動；示範用的原型在 `daemon/src/lifecycle/actor_runtime_eval_prototype.rs`，
整檔 `#[cfg(test)]`（比照 issue #81 的 `native_transport_prototype.rs`），不進 `cargo build`。

## 讀了什麼

- `1a2aa019`（daemon/src/lifecycle/turn_controller.rs）：`LEGAL_EDGES` 是 `turns.status` 合法邊
  的唯一定義，生成一句 `CREATE TRIGGER ... RAISE(ABORT)` 裝在 `turns` 上；`set_status` 是給新程式碼
  用的 CAS 門。既有二十來處各自帶 guard 的 `UPDATE` **原封不動**——trigger 是它們的下限，不是取代。
- `77e64b35`（daemon/src/supervisor/assignment_state.rs）：`allowed(from, to)` 是交辦狀態的合法轉移
  表，`sources_for(to)` 讓 SQL 的 `WHERE status IN (...)` 從表算出來，不必每個呼叫端各自抄一份清單。
  同樣不是 dispatcher，是給既有 SQL 呼叫端用的一個純函式。
- `12208eda`（daemon/src/mission/workflow.rs）：`ensure_can_assign`／`ensure_can_complete` 兩道
  409 閘門，共用「這個任務還開著的交辦」同一份查詢，把 SPEC §18.14 那條「同時只有一件」規則從
  「AGM 要記得」變成「daemon 擋」。

三者共同的形狀：**狀態轉移的合法性，被搬進一個查得到、測得到、且與『誰在呼叫』無關的地方**（DB
trigger 或純函式），但**執行**轉移的程式碼——`queue::flush_queued_locked`、`store::mark_delivered`、
`mission::api::post_complete` 這些——完全沒有被重寫成走某個 dispatcher／actor。這正是回答 issue #77
第一個問題的關鍵：collectivization 已經發生，但發生在「規則」那一層，不是「執行」那一層。

## 逐題回答

### 現有 per-bot mutex + centralized controller 是否已足夠？

「centralized controller」目前不存在（也不需要存在）——`app.bot_lock`（`daemon/src/state.rs:272`）
仍然是分散在 12 個檔案裡的 50 個呼叫點各自 `lock().await` 之後做自己的事，doc comment 自己講得
明白：「start / stop / prompt / hook matching / spool replay / reconcile all take it.」。這聽起來
像是問題，但**真正扛住正確性的不是這把鎖**：

- `turns_one_in_flight`／`turns_one_queued` 這兩個 UNIQUE INDEX（`daemon/src/db.rs`）讓「兩邊都以為
  自己搶到了」變成 SQL 錯誤，不是靜默的資料損毀——`queue.rs:5-8` 的 doc comment 直接寫「the one-in-flight
  / one-queued unique indexes make a lost race an error」。
- 今晚新增的 `turns_status_transition` trigger 讓「非法轉移」不管從哪條路徑來都是 `RAISE(ABORT)`。
- `supervisor::assignment_state`／`mission::workflow` 把另外兩張表的等價保證做成純函式。

`bot_lock` 的角色其實是「避免同一顆 bot 的兩個操作互相踩腳、浪費工作」的**效能/秩序**優化，不是
「防止資料損毀」的最後一道防線——那道防線在 DB 裡，鎖拿不到、行程重啟、鎖的實作換掉，DB 的
constraint 都還在。這是「已經足夠」的判斷基礎：不是「沒有更好的方法」，是「真正需要的保證已經在
更便宜的地方拿到了，剩下的鎖只是排隊機制，不是正確性機制」。

### actor 是否能實際減少 lock ordering / queue flush / interrupt / run-exit races？

**Queue flush／run-exit**：這兩類已經被 DB 的 UNIQUE INDEX + 今晚的 trigger 擋住（見上）。actor
會把「排隊」這件事從「等 `bot_lock`」換成「等 mailbox 輪到自己」，但保護資料正確性的仍然是同一組
DB constraint——actor 沒有拿掉它們，也不能拿掉，因為 DB 檔案本身不知道呼叫端是不是 actor。

**Interrupt**：`interrupt_grace`（`daemon/src/lifecycle/interrupt_grace.rs`）處理的是「使用者按了
中斷，排隊的派工要等到真的閒置滿寬限才送」，本質是一個**跨事件的時間視窗判斷**（連續閒置多久、
有沒有更新的使用者輸入），不是鎖爭用問題。actor 化不會讓這個判斷變簡單：無論是「進 mailbox 前
查一次剩餘寬限」還是「actor 收到訊息時查一次」，要查的是同一份 DB 狀態（`interrupted_at`、
最後一次活動時間），時間窗邏輯本身不會因為換了執行模型而消失。

**Lock ordering**：這是三者裡**唯一** actor 換不掉底層難題的。`api.rs:1348` 的 `lock_bots_in_order`
明講「刪除是唯一會同時持多把（bot_lock）的路徑」，而且是繞過死鎖才長成現在這樣（`sol 五輪`：
`delete_bot` 先鎖 parent、`delete_project` 先鎖 child，ULID 不保證誰比較小，兩條路徑對開時互等）。
換成「每個 bot 一個 actor」之後，`delete_project` 要嘛（a）依然需要同時協調 parent／child 兩個
actor 各自確認「我這邊沒事了」——這正是下面原型示範的：**兩個獨立單元互相等對方確認，不管底層是
mutex 還是 mailbox，都是同一個環狀等待的形狀，一樣需要外部排序或逾時**；要嘛（b）把整個刪除操作
收斂成單一協調者硬做（等於把 `lock_bots_in_order` 原封不動搬進一個新的協調層，換了名字沒換做法）。
actor 沒有提供第三個選項。

### daemon restart 時 actor mailbox 如何與 durable state 配合？

這是**最大的隱藏成本**，也是原型實際跑給你看的那個結論：**actor 的 mailbox／內部狀態是揮發性
的**——daemon 重啟，任何還沒被明確持久化的訊息就是沒了。這代表 actor 完全不能取代現有「先寫 DB
再算數」的路徑，只能疊加在它**前面**：呼叫端把命令丟進 channel、actor 收下、actor 再去寫 DB——
多了一段排隊延遲，卻沒有拿掉任何一次 DB 寫入，因為那次寫入本來就是唯一能撐過重啟的東西。

這點被今晚另一顆commit（issue #75，`到期動作`）獨立印證：daemon 現在有六張表個別記著「下次
什麼時候要做什麼」（`turns.next_flush_at`、`supervisor_assignments.next_attempt_at`、額度
`resume_at`、`supervisor_inbox.notify_next_at`、`supervisors.watchdog_next_at`、
`hook_events.next_attempt_at`），每張表都有自己的「重啟後從 DB 重掛」邏輯（`reconcile::rearm_progress`、
`herdr_maintenance::arm_on_startup`、`hook_inbox` worker），`docs/SPEC.md` 原句：「記憶體 timer
只是加速：重啟後...依 DB 重掛或掃回來，重掛是冪等的」。這是跟 actor mailbox **完全相反**的哲學：
記憶體裡的任何東西都只是「加速用的影子」，真正的狀態一律先落地。導入 actor 等於在這套已經自洽的
「DB 是唯一真相」架構旁邊，再蓋一層需要自己的持久化／重放策略的易失狀態——多一種故障模式，
不是少一種。

### remote host / hook / scheduler event 如何 ingress？

現況：11 個檔案（`api.rs`、`hookrecv.rs`、`lifecycle/{poller,prompt,queue,screen,slash,start,stop,
stuck_turns}.rs`、`reconcile.rs`、`default_session.rs`）各自直接讀寫 `app.db`，需要序列化時各自
`bot_lock().await`。這些呼叫端沒有共同的「訊息」概念——hook 收到的是 JSON payload、HTTP 是解析過
的請求體、poller／reconcile 是週期性掃描的結果、scheduler 到期動作是 DB 裡的一列。要接進 actor，
這 11 個檔案的每一個呼叫點都要改寫成「組一個訊息、送進對應 bot 的 channel」，而且送出之後原本
「這次操作完成了嗎」的同步語意（HTTP handler 要回應使用者、hook 要回 200）要嘛變成等 actor 回覆
（等於沒有拿掉同步等待，只是多繞一手），要嘛變成 fire-and-forget（等於放棄現有「操作完成才回應」
的保證）。這是這次評估裡改動面最大的一項，而且沒有一個問題會因此被解決——上面四題已經說明
真正的正確性保證在 DB 層，不在誰持有鎖／誰收訊息。

### 是否會讓 hot-path latency／debugging 反而更差？

**Latency**：多一段 channel send + 等 actor 排到自己的延遲，換不到任何現有 DB 寫入的省略（見上）。
純粹加法。

**Debugging**：這個專案的偵錯與測試風格高度依賴「直接對 SQLite 下 SQL 查任何一列的當下狀態」——
今晚寫的每一條新測試（`turn_controller`／`assignment_state`／`mission::workflow`，以及本次 fix
bot 在 #72／#85／#92／#95 寫的測試）都是 `sqlx::query_scalar`／`PRAGMA table_info` 直接戳表，
包括生產環境排錯（`sqlite3 ~/.config/agents-manager/agents-manager.sqlite3` 手動查）。actor 會把
一部分狀態搬進 mailbox／task 內部變數，變成除非發一個「回報你的內部狀態」的訊息、或整個換成
event-sourcing／snapshot 機制，否則外部看不到——這既是額外的架構負擔，也直接跟這個 codebase 已經
驗證有效、成本很低的偵錯方法背道而馳。

## 什麼情況會改變這個結論

- 如果將來多顆 daemon 行程真的需要同時管同一份 SQLite（目前是硬性禁止的，見 CLAUDE.md／SPEC 多處
  「絕對不要起第二顆 daemon」），單一 in-process mutex 就不夠了，那時候的解法也不是 actor，
  而是換掉共享儲存本身（例如換成真正支援多寫入者協調的資料庫）。
- 如果 `bot_lock` 實測真的成為熱路徑瓶頸（目前沒有任何量測支持這個假設；本次評估也沒有新增量測，
  因為現有模型的瓶頸從未被回報過）。
- 如果需要 actor 提供的東西不是「正確性」而是「背壓／優先權排程」（例如同一顆 bot 排隊的操作要
  依優先權而非到達順序處理）——這是 actor/queue 模型的正經強項，但目前沒有任何 issue 提出這個
  需求。

## 驗證

- 新增 `daemon/src/lifecycle/actor_runtime_eval_prototype.rs`（`#[cfg(test)]`，不進正式二進位），
  3 條測試：一條示範 actor mailbox 收下訊息到真正落地之間有易失空窗，重啟（丟掉 task）會讓那個空窗
  裡的訊息完全消失；一條對照 `turn_controller::set_status` 本身就是原子的持久化操作，沒有這種空窗；
  一條把 `bot_lock` 目前的呼叫檔案清單（11 個）與「刪除是唯一會同時持多把」這個事實釘進測試，
  不讓上面的敘述性論證跟程式碼實際的形狀脫鉤。
- 反向變異、daemon 測試套件全綠、`cargo clippy --all-targets` 結果見 issue 留言。
