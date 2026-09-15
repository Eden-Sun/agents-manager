# AGM 運維腳本（scripts/ops/）

正式環境跑的那幾支腳本原本只存在於 `~/.config/agents-manager/supervisor/AGM/bin/`，沒有進版控：
沒有 review、沒有歷史、改壞了也沒得比對。這個目錄是它們在 repo 裡的版本。

**這裡的檔案不會自動安裝。** daemon 只把 `bin/agm`（`scripts/agm.py`）部署到總管 cwd；
這些 kick 腳本要不要裝、什麼時候裝，由 AGM 決定並在有窗口的時候執行。

## daemon-update-kick.sh

例行更新：正式 daemon 的 release binary 落後 `origin/main` 時，申請核准、取得 rebuild 租約，
再把重建重啟任務派給建置 child。

**三個觸發條件**：整點的例行檢查（launchd 每 5 分鐘跑一次，分鐘 < 5 的那一輪）、
**重建申請集滿門檻**（`AGM_REBUILD_THRESHOLD`，預設 5；使用者 2026-09-14），
或**最早一筆申請已經等超過上限**（`AGM_REBUILD_MAX_WAIT_MIN`，預設 30 分鐘；使用者 2026-09-15——不能卡著等湊滿）。
三個都不成立就立刻 `exit 0`，連 `git fetch` 之後的判斷都不做。同一個 `origin/main` 已經派過就不會重派，
所以「等太久」在部署卡住時每 5 分鐘觸發一次也只會記一行「已經派過，跳過」。

「申請」的定義：`purpose=rebuild` 的核准申請（`agm approval request`／`POST /api/supervisor/approvals`），
建立時間晚於上次真的上線（`daemon-update.built` 的 mtime）、狀態是 `pending` 或 `approved`；
`denied` 不算，同一個 requester 對同一個 commit 重複申請算一筆。bot 在對話裡的口頭申請由 AGM 補一筆
approval，所以 approval 表就是唯一真相。數不出來（端點壞了、格式不符）就當 0，退回純整點的舊行為。
網頁左上角 RAM 那一格旁邊的 chip 顯示同一個數字（`web/src/api/rebuildRequests.ts`）。

環境變數：

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state 都在這） |
| `AGM_REPO` | `~/project/agents-manager` | 要比對的 repo |
| `AGM_BUILD_BOT` | （無） | 建置 child 的 bot id。**沒設就整支跳過**——寧可不做，也不要改派給使用者的 bot |
| `AM_AGENT_NAME` | `daemon-update-kick` | 租約 owner |
| `AGM_REBUILD_THRESHOLD` | `5` | 累積幾個重建申請就不等整點，立刻檢查 |
| `AGM_REBUILD_MAX_WAIT_MIN` | `30` | 最早一筆重建申請等超過幾分鐘就不等整點（申請沒湊滿也一樣） |
| `AGM_TEST_MINUTE` | （無） | 只給隔離測試用：假裝現在是第幾分鐘 |

跟 2026-09-12 之前那份的差別：

1. 未結案判斷用 `agm assignments --open`，含 `awaiting_review`。回合跑完但沒人驗收時不會再疊一筆。
2. 「會影響 binary 的路徑」跟 daemon 對齊（`agm build-inputs`），不再漏掉 `include_str!` 進來的
   persona 與 `scripts/agm.py`。問不到端點時用保底清單。
3. 空閒判斷改成 `lease safety`（等窗口）＋ `lease acquire`（在同一個鎖裡重驗並拿走窗口）。
   依 SPEC §18.2，建置前排除建置 child 與 runtime.json 的 `manager_bot_id`；safety 與 acquire 都帶同一份兩顆 `--exclude-bot`；CLI 以 `?exclude=<id,id>` 傳給 safety API。新版回應會列出 `excluded_bot_ids`，腳本直接採用 daemon 判定；缺少該欄或名單不符就跳過，不自行過濾快照。其他 bot 仍受保護；runtime 缺少有效管理員 ID 就跳過並記錄原因。這個排除僅用於 rebuild，restart 另行核准。
   拿著 `restart` 租約期間 supervisor assignment 派送會暫停；這不是所有 prompt 路徑的全域互斥鎖，正式替換前仍須由 AGM 重驗窗口。
4. `daemon-update.approval.json` 保存同一完整 commit 與申請者的核准 ID，下個整點接續查核。pending、denied、revoked 不另建申請；過期後下輪才重新申請。查派工或核准失敗時停止，不當作無工作或已獲准。
5. `daemon-update.lock` 防止腳本重疊執行。若程序被強制終止留下鎖，由 AGM 確認沒有執行者後移除。核准狀態檔損毀或 ID 不在查詢結果中也交 AGM 檢查，不自動繞過。
6. AGM 雙角色（SPEC §18.15）：idle 檢查另外排除協調者（`agm responder show` 有 `bot_id` 時）；runtime.json 有 `role` 才帶 `assign --review-by <role>`
   （巡檢目錄＝`patrol`，更新結果回巡檢驗收）。舊部署兩者都沒有，行為照舊。

### 隔離測試

不要對正式 daemon 測。開一個獨立 daemon 與獨立資料目錄：

```sh
AM_DATA_DIR=/tmp/am-ops-test ./target/release/agents-managerd serve --port 7799 &
mkdir -p /tmp/am-ops-test/supervisor/AGM/bin
# 把 bin/agm 指到測試 daemon（runtime.json 的 daemon_url 寫 127.0.0.1:7799）
AGM_DIR=/tmp/am-ops-test/supervisor/AGM AGM_REPO=$PWD AGM_BUILD_BOT=<測試 bot> \
  bash scripts/ops/daemon-update-kick.sh
cat /tmp/am-ops-test/supervisor/AGM/daemon-update.log
```

### 正式安裝（需要 AGM 核准）

```sh
install -m 755 scripts/ops/daemon-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

launchd：`com.agm.daemon-update` 改成每 5 分鐘跑一次（`StartInterval 300`），由腳本自己判斷
「整點、集滿門檻，或等太久」；`AGM_BUILD_BOT`（必要）與 `AGM_REBUILD_THRESHOLD`／`AGM_REBUILD_MAX_WAIT_MIN`（可選）放 `EnvironmentVariables`。

## 租約管得到什麼、管不到什麼

租約約束的是**走 API 與這些腳本的路徑**：

- 這些腳本、`bin/agm`、`/api/supervisor/*` 的重建與重啟申請。
- 拿著 `restart` 租約期間，daemon 的 assignment 派送會 hold 住（不丟工作，等窗口結束再送）。

- **只管 supervisor 的 assignment 派送**：`POST /api/bots/{id}/prompt` 沒有被 gate。
  使用者自己打字、PM 派下一棒，在窗口期間照樣進得去。

管不到的：

- 這台機器上任何一個 shell 直接 `kill` daemon、自己 `cargo build --release`、或用別的方式換掉
  binary。沒有 OS 層的鎖能從 daemon 這裡強制，**這是已知邊界，不要在文件或回報裡假裝有**。
- 因此規範仍然有效：要重啟、要 release rebuild，先問 AGM（見 repo 的 `CLAUDE.md`）。租約是讓
  「問過了」這件事在執行期間持續成立，不是替代它。

此 safety API／內嵌 CLI 更新需要重建並部署 daemon，再由 AGM 部署 kick。升級前暫保留正式環境的 client-side 過濾熱修；repo 本版不再自行排除，舊端點會使本版跳過派工。
