# AI Team 調度第一版設計計畫（2026-09-09）

狀態：設計提案，尚未實作或驗證。範圍為單一 team、同一 issue 內的任務依賴、優先順序與 PM 重規劃；沿用現有 PM agent。

## 1. 目標與現況

目標：PM 一次描述 A、B 可並行，C 必須等 A、B 合併；daemon 正確派工，出錯時能解釋等待原因並請 PM 調整。

現有實作：
- `daemon/src/team.rs` 的 PM persona 已要求拆工並透過 `dispatch` 派工。
- `team_sched.rs::DispatchItem` 有 brief、files、to、issue，沒有依賴或優先順序。
- `dispatch` 逐筆寫入，允許部分成功；檔案重疊只有警告。
- `fill_issue` 依序選 queued、尚未指派且符合指定 worker 的任務；在實際派工時切分支。
- SQLite、team_events、outbox、額度與時間限制繼續作為執行基礎。

AI 提出計畫，Rust 檢查與執行。第一版不增加獨立的全域 AI 調度員，也不改跨主機、跨 team、模型或身份分配。

## 2. 預期行為

例如 PM 拆出「A：API」「B：獨立 UI 元件」「C：整合驗證」，C 依賴 A、B：
1. 併行數 2：A、B 開工，C 顯示「等待 A、B 合併」。
2. A report done 或 reviewer approve，C 仍不能開始；A、B 都進 merged 才滿足條件。
3. C 從當時的整合分支切出，包含 A、B 已合併內容。
4. A failed 或 skipped，C 保留 queued，顯示依賴失敗；同一 issue 其他獨立任務可繼續，既有團隊級暫停規則仍有效。
5. PM 收到一次依賴失敗通知，可提出替代任務與 C 的依賴調整。系統不自動移除失敗依賴。

## 3. 資料與協定

建議新增：
- `team_tasks.priority`：整數 0–3，3 最高，舊資料預設 0。
- `team_tasks.acceptance_json`：驗收條件字串陣列，舊資料預設空陣列；傳給 worker 與 reviewer，不當作已通過驗收的證據。
- `team_task_dependencies(task_id, depends_on_task_id)`：複合主鍵、外鍵及反向查詢索引。
- 每個 issue 的 `schedule_version`：單調遞增；派工計畫、任務指派、任務狀態及相關調度設定變更時更新，用於拒絕過期提案。
- `team_schedule_proposals`：提案 ID、team/issue、来源 turn、預期版本、完整 payload、reason、狀態與拒絕原因；來源 turn 加唯一約束防止重放。

新增 `plan` 動作提供整批交易語意。保留既有 `dispatch` 的相容行為；兩者寫入後使用相同的補位流程。

提案格式示意（新協定，尚不存在）：

```json
{
  "action": "plan",
  "issue": 48,
  "expected_version": 12,
  "reason": "整合驗證需要 API 與元件都已合併",
  "create": [
    {"key": "api", "title": "API", "brief": "實作 API", "priority": 2, "depends_on": [], "acceptance": ["API 測試通過"]},
    {"key": "ui", "title": "UI 元件", "brief": "實作獨立元件", "priority": 1, "depends_on": []},
    {"key": "integration", "title": "整合驗證", "brief": "串接並驗證", "priority": 3, "depends_on": [{"key": "api"}, {"key": "ui"}]}
  ],
  "update": []
}
```

`key` 僅在同一提案內解析，落地轉成 task ID；既有任務用 `{"task_id":"..."}` 引用，避免與新任務 key 混淆。`update` 指定 task_id，可改 priority、depends_on，必須提供 reason；第一版不修改已收單任務的 brief、files、驗收條件或指定 worker。

驗證規則：
- 僅該 team 的 PM 能送 plan，所有任務須屬同一個 working issue。
- 拒絕不存在、跨 issue、自我依賴、循環依賴、重複 key、重複 update 目標和越界 priority。
- 修改只允許 queued 且 worker_bot_id 為空的任務；已保留 worker 或已產生派工 relay 的任務不可修改。
- 依賴可指向新任務或既有任務，但不能新增指向 failed/skipped 的依賴。
- 在交易內重讀版本與任務狀態，整批驗證後才寫入，失敗整批不套用。
- 同一來源 turn 重放回傳原結果；仍保留既有重複工作檢查，不能用新 key 規避。
- 資料庫舊表重建遷移完成後才新增欄位與依賴表，避免舊版 nullable-worker 遷移丟欄位；清理流程配合外鍵。

## 4. 排程與重規劃

`fill_issue` 先排除依賴尚未全部 merged 的任務，再按 priority 降序、seq 升序選取；保留指定 worker、每 worker 一個未完成任務、併行上限及既有 gates。

多 issue 模式仍按現有 issue 隔離執行；priority 第一版只在同一 issue 內比較。有限 issue 佇列內同優先序維持 FIFO；持續插入高優先任務造成飢餓的情況先記錄等待時間，不在首版加入自動 aging。

API 提供衍生的 `ready`、`waiting_for`、`dependency_failed`，不增加 task state CHECK 值。依賴阻塞不算完成，不能讓 PM 提早 done。

依賴失敗通知以「下游任務＋上游任務＋失敗事件」去重，透過既有 outbox 傳給 PM。PM 取得 issue 的任務、依賴、可用 worker、限制與 schedule_version 快照；不需要每 20 秒呼叫 AI。暫停時可記錄通知，送出仍受既有 gates 控制。

同一阻塞原因最多接受兩輪自動重規劃嘗試（包含被拒提案），之後明確等待使用者；同時遵守原本 relay/time/quota 預算。PM 無回應時顯示等待 PM，獨立任務按既有規則執行。

版本失配回傳最新快照，要求重新提案，不自動把旧提案套到新狀態。任務指派與提案套用須在相同序列化邊界或 DB 交易下互斥，不能只依賴記憶體中的單一 scheduler。

## 5. 導入模式與 UI

新增 team 設定 `scheduling_mode = legacy | suggest | auto`，預設 legacy。
- legacy：原有 PM 協定及排程；不發送新 plan 指令。
- suggest：新增 plan 先落成 pending 提案，使用者可檢視並套用／拒絕；既有任務繼續排程。新任務未套用前不派工。
- auto：有效 plan 自動套用，仍受所有原有執行閘門限制。

模式切換不刪除已套用的依賴或優先序；即使切回 legacy，已存在的依賴仍須被遵守。pending 提案不因切 auto 自動執行，須重新驗證並明確處理。

TeamPanel 顯示任務優先序、等待對象、依賴失敗原因與驗收條件；提案卡列出新增任務、依賴與優先序的前後差異、PM 理由及版本失效提示。時間軸記錄提案、套用、拒絕與派工原因。前端不得自行猜 ready。

建議 API：`GET /teams/:id/schedule-proposals`、`POST /teams/:id/schedule-proposals/:proposal_id/apply`、`POST /teams/:id/schedule-proposals/:proposal_id/reject`。套用要求 expected_version；過期回 409，不能透過 UI 繞過狀態驗證。

## 6. 實作切分

1. **資料與純規則**：db.rs 遷移；新增 team_schedule.rs 放圖驗證、ready 判斷與排序；補舊 DB 遷移測試。
2. **排程整合**：team_sched.rs 接入依賴與 priority；先用程式測試送資料，確認現有無依賴任务維持行為。
3. **PM 與提案**：plan 解析、交易、版本、提案儲存與去重；team.rs 更新 persona、快照與模式設定；api.rs 接入查詢與套用。
4. **UI 與契約**：api types/normalize/mock、TeamPanel 提案卡與任務等待原因；同步 API.md、SPEC-team.md。
5. **有限自動化**：依賴失敗通知、重規劃上限與 auto 模式；先在隔離測試 team 驗證。

目前工作目錄存在其他功能的未提交變更；實作時逐檔協調，不覆蓋或提交無關修改。

## 7. 驗收與成效

必要測試：
- A、B→C 的分支包含前置合併；reported/approved 不提前解鎖。
- 高優先但依賴未滿足的任務，不擋住低優先且 ready 的任務。
- 同優先序 FIFO；指定忙碌 worker 不誤派其他人；沒有可派任務時不無限通知 PM。
- 自我依賴、循環、跨 issue、壞引用整批拒絕，沒有部分任務殘留。
- failed/skipped 不解鎖；重規劃將依賴改到替代任務後正確推進。
- suggest 不執行未批准提案；批准時若版本過期則拒絕；worker 已保留時不得修改。
- hook 重放、重啟、重複套用及提案／派工競態不產生重複任务或 relay。
- 舊資料遷移、無依賴排程、paused、quota、dispatch gate、done 守門與多 issue 隔離均通過回歸測試。

檢查：`cargo test -p agents-managerd`；前端使用 `bunx tsc -p tsconfig.app.json --noEmit`、相關 node 測試及 build。純計畫階段不執行這些測試。

先記錄既有策略的等待時間、每 issue 人工介入、衝突／重工、完成耗時與 relay 次數；能取得可靠用量時再比較 token。歷史事件回放只能驗證規則、估計排程差異，不能證明改派後的實際完成時間。用隔離 team 的試跑驗證收益，再決定是否擴大 auto 模式。

第一個可交付里程碑：suggest 模式下，PM 一次提出 A、B→C，UI 顯示依賴與提案，使用者套用後正確依序執行，重啟也不重複派工。
