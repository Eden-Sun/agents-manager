# AGM 運維腳本（scripts/ops/）

正式環境跑的那幾支腳本原本只存在於 `~/.config/agents-manager/supervisor/AGM/bin/`，沒有進版控：
沒有 review、沒有歷史、改壞了也沒得比對。這個目錄是它們在 repo 裡的版本。

**這裡的檔案不會自動安裝。** daemon 只把 `bin/agm`（`scripts/agm.py`）部署到總管 cwd；
這些 kick 腳本要不要裝、什麼時候裝，由 AGM 決定並在有窗口的時候執行。

## daemon-update-kick.sh

例行更新：正式 daemon 的 release binary 落後 `origin/main` 時，申請核准、取得 rebuild 租約，
再把重建重啟任務派給建置 child。

**四個觸發條件**：整點的例行檢查（launchd 每 5 分鐘跑一次，分鐘 < 5 的那一輪）、
**重建申請集滿門檻**（`AGM_REBUILD_THRESHOLD`，預設 3；使用者 2026-09-14 訂 5、2026-09-16 降成 3）、
**最早一筆申請已經等超過上限**（`AGM_REBUILD_MAX_WAIT_MIN`，預設 30 分鐘；使用者 2026-09-15——不能卡著等湊滿），
或**自己有還在等的核准**（pending／approved、沒過期：在等 AGM 裁示或安全窗口）。
四個都不成立就立刻 `exit 0`，連 `git fetch` 之後的判斷都不做。同一個 `origin/main` 已經派過就不會重派，
所以「等太久」在部署卡住時每 5 分鐘觸發一次也只會記一行「已經派過，跳過」。

「申請」的定義：`purpose=rebuild` 的核准申請（`agm approval request`／`POST /api/supervisor/approvals`），
建立時間晚於上次真的上線（`daemon-update.built` 的 mtime）、狀態是 `pending` 或 `approved`、**還沒過期**；
`denied` 不算，同一個 requester 對同一個 commit 重複申請算一筆，**這支腳本自己（`AM_AGENT_NAME`）的申請不算**
（review2 2026-09-16：算進去的話自己申請、30 分鐘後自己觸發「等太久」，每 5 分鐘一輪、main 一動就再對協調者開一筆）。bot 在對話裡的口頭申請由 AGM 補一筆
approval，所以 approval 表就是唯一真相。數不出來（端點壞了、格式不符）就當 0，退回純整點的舊行為。
網頁左上角 RAM 那一格旁邊的 chip 顯示同一個數字（`web/src/api/rebuildRequests.ts`）。

環境變數：

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state 都在這） |
| `AGM_REPO` | `~/project/agents-manager` | 要比對的 repo |
| `AGM_BUILD_BOT` | （無） | 建置 child 的 bot id。**沒設就整支跳過**——寧可不做，也不要改派給使用者的 bot |
| `AM_AGENT_NAME` | `daemon-update-kick` | 租約 owner |
| `AGM_REBUILD_THRESHOLD` | `3` | 累積幾個重建申請就不等整點，立刻檢查 |
| `AGM_REBUILD_MAX_WAIT_MIN` | `30` | 最早一筆重建申請等超過幾分鐘就不等整點（申請沒湊滿也一樣） |
| `AM_MAINTENANCE_ESCALATE_MINS`（daemon 端） | `30` | 已核准的窗口等超過幾分鐘，daemon 就縮小封鎖面：思考中的 bot 不再擋，只擋送達臨界區／租約／讀不到畫面（SPEC §18.10）。設在 daemon 的環境，不是這支腳本 |
| `AGM_LOCK_STALE_SECS` | `120` | 鎖沒有可查的執行者時，超過這麼久就當殘留回收 |
| `AGM_LOCK_HUNG_SECS` | `3600` | 執行者還活著但卡了這麼久：推 `ops_alert` 喊人（不搶鎖） |
| `AGM_TEST_MINUTE` | （無） | 只給隔離測試用：假裝現在是第幾分鐘 |

跟 2026-09-12 之前那份的差別：

1. 未結案判斷用 `agm assignments --open`，含 `awaiting_review`。回合跑完但沒人驗收時不會再疊一筆。
2. 「會影響 binary 的路徑」跟 daemon 對齊（`agm build-inputs`），不再漏掉 `include_str!` 進來的
   persona 與 `scripts/agm.py`。問不到端點時用保底清單。
3. 空閒判斷改成 `lease safety`（等窗口）＋ `lease acquire`（在同一個鎖裡重驗並拿走窗口）。
   依 SPEC §18.2，建置前排除建置 child 與 runtime.json 的 `manager_bot_id`；safety 與 acquire 都帶同一份兩顆 `--exclude-bot`；CLI 以 `?exclude=<id,id>` 傳給 safety API。新版回應會列出 `excluded_bot_ids`，腳本直接採用 daemon 判定；缺少該欄或名單不符就跳過，不自行過濾快照。其他 bot 仍受保護；runtime 缺少有效管理員 ID 就跳過並記錄原因。這個排除僅用於 rebuild，restart 另行核准。
   拿著 `restart` 租約期間 supervisor assignment 派送會暫停；這不是所有 prompt 路徑的全域互斥鎖，正式替換前仍須由 AGM 重驗窗口。
4. `daemon-update.approval.json` 保存同一完整 commit 與申請者的核准 ID，下一輪接續查核。pending、denied、revoked 不另建申請；過期、已被用掉（`consumed`，例如上一個窗口過期沒交還）或被取代時**同一輪**重新申請。查派工或核准失敗時停止，不當作無工作或已獲准。
   `origin/main` 動了但 `build-inputs` 路徑沒變（docs-only）：沿用原核准，acquire 的 `--commit` 用核准那一顆，不為了建出一樣的東西再叫醒協調者。
   真的動到要建的東西：新申請帶 `--supersedes <舊 id>`，daemon 把舊的標 `superseded`、等待時間接過去（升級計時不因為 main 動了就歸零，SPEC §18.10）；舊的 `bin/agm` 不認得這個旗標時照舊開新的一筆。
7. `lease_token` **不進派工正文**：寫進 `daemon-update.lease-token`（權限 600），正文只給 `--lease-token "$(cat …)"`。正文會出現在 assignments API、建置 child 的對話紀錄與這份 log。
5. `daemon-update.lock` 防止腳本重疊執行，鎖裡寫 pid 與時間。執行者已經不在（強制關機、斷電、SIGKILL）就**自己回收**並接手這一輪；
   還活著但卡超過 `AGM_LOCK_HUNG_SECS`（預設 3600 秒）不搶它的鎖，改推一則 `ops_alert` 給 AGM。核准狀態檔損毀、或核准 ID 查不到
   （先用 `approval list --id` 查，清單只回最新 100 筆）一樣停住並推 `ops_alert`，不自動繞過——以前這些只寫 log 就 `exit 0`，
   換版流程永久、靜默地停住（review3 c1 M1）。
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
install -m 644 scripts/ops/daemon-update-task.md ~/.config/agents-manager/supervisor/AGM/
```

任務內容在 `daemon-update-task.md`（kick 會把它當成派工正文的開頭，末尾再補這一輪的 sha／核准／租約）。
這份是**來源檔**：改規則改這裡再 install，不要只改 AGM 目錄裡那份，否則下次有人從 repo 安裝就把規則改回去了。

launchd：`com.agm.daemon-update` 改成每 5 分鐘跑一次（`StartInterval 300`），由腳本自己判斷
「整點、集滿門檻，或等太久」；`AGM_BUILD_BOT`（必要）與 `AGM_REBUILD_THRESHOLD`／`AGM_REBUILD_MAX_WAIT_MIN`（可選）放 `EnvironmentVariables`。

## claude-release-kick.sh

Claude Code 換版就派 AGM 解析新版有什麼用得上的，AGM 的回覆就是給使用者的通知（使用者 2026-09-16）。
唯讀、只派工，不 build 不重啟。任務內容在 `claude-release-task.md`（安裝到 AGM 目錄）。

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state） |
| `CLAUDE_VERSIONS_DIR` | `~/.local/share/claude/versions` | 版本目錄；目錄名就是版本號，最新的那個是現在會跑的 |
| `AGM_RELEASE_BOT` | `runtime.json` 的 `release_bot_id`，沒有就 `responder_bot_id`（協調者） | 派給誰；**不能是巡檢自己**（daemon 擋總管對自己下交辦）。查不到就跳過，不亂派給別的 bot |

狀態檔 `claude-release.last`＝已經解析過的版本；派工成功才寫。隔離測試：`bash scripts/ops/claude-release-kick_test.sh`（假的 AGM 目錄、版本目錄與 `bin/agm`）。

安裝（需要 AGM 核准）：

```sh
install -m 755 scripts/ops/claude-release-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/claude-release-task.md ~/.config/agents-manager/supervisor/AGM/
```

launchd：`com.agm.claude-release`，`StartInterval 1800`，`ProgramArguments = [/bin/bash, …/bin/claude-release-kick.sh]`。

## herdr-update-kick.sh

herdr 有新版時整理出「對我們有沒有用、會不會壞」，派給 AGM 排程處理（issue #66，SPEC §18.2b）。
唯讀、只派工，**不升級、不重啟 herdr server**——真的升級一律要 AGM 核准後走 §6.5.2 的維護模式手動做。
版本比較與 CHANGELOG 段落擷取交給 `agents-managerd herdr-update-check`（`daemon/src/herdr_update.rs`），
這支腳本只負責問本機版本、問 GitHub 最新穩定版、抓 CHANGELOG 全文，再照 JSON 決定要不要派工。

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state） |
| `AGM_REPO` | `~/project/agents-manager` | 找 `target/release/agents-managerd` 的 repo（可被 `AM_BINARY` 整個蓋過） |
| `AM_BINARY` | `$AGM_REPO/target/release/agents-managerd` | 呼叫 `herdr-update-check` 的二進位路徑 |
| `HERDR_REPO` | `herdrdev/herdr` | 查最新穩定版與 CHANGELOG 的 GitHub repo |
| `HERDR_CHANGELOG_URL` | `https://raw.githubusercontent.com/<HERDR_REPO>/master/CHANGELOG.md` | CHANGELOG 全文來源（測試用 `file://` 亦可） |
| `AGM_HERDR_UPDATE_BOT` | （無） | 派給誰；沒設就退回 `runtime.json` 的 `herdr_update_bot_id` → `release_bot_id` → `responder_bot_id` |

狀態檔 `herdr-update.last`＝已經派過工的版本，派工成功才寫；同一版不重派。隔離測試：
`bash scripts/ops/herdr-update-kick_test.sh`（假的 `herdr`／`gh`／`agents-managerd`／`bin/agm`，`file://` CHANGELOG）；
`herdr-update-check` 本身的版本比較與 CHANGELOG 段落擷取正確性由 `cargo test -p agents-managerd` 釘住
（`daemon/src/changelog.rs`、`daemon/src/herdr_update.rs`），不在這支腳本測試裡重測。

安裝（需要 AGM 核准）：

```sh
install -m 755 scripts/ops/herdr-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

launchd：`com.agm.herdr-update`，`StartInterval 86400`（每天一次），`ProgramArguments = [/bin/bash, …/bin/herdr-update-kick.sh]`。

**相容性驗證沙箱（issue #66 做法 §3）沒有做**：理由與成本分析寫在 SPEC §18.2b 最後一段——沙箱要嘛只驗協定形狀
驗不到真正的行為差異，要嘛要重建一份接近正式環境的執行環境、成本不小，留給 AGM 收到交辦、看過那一版實際改了什麼再決定要不要做。

## release-triage-kick.sh

上游新版分診（issue #204）：claude／codex 每出一版，把 changelog 裡「可能該採用、或必須提防」的條目派給專責 bot 逐條下 verdict，
該處理的由 daemon 開成 GitHub issue。唯讀、只派工，**不升級、不改設定、不重啟**——升級照舊走 §6.9。
版本比較、切 changelog、分桶（dropped／kept／unmatched）全部交給 `agents-managerd release-triage-check --kind <k> --json`（daemon 端），
這支腳本不做版本比較也不切段，只照回來的 JSON（`{"kind","from","to","pending":[{"version","kept","unmatched","dropped_count"}]}`）決定要不要派。
`pending` 是空的就安靜結束、不寫 log。任務內容在 `release-triage-task.md`（kick 把它當交辦正文開頭，後面接本次 JSON）。

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state 都在這；task 檔也放這） |
| `AGM_REPO` | `~/project/agents-manager` | 找 `target/release/agents-managerd` 的 repo（可被 `AM_BINARY` 整個蓋過） |
| `AM_BINARY` | `$AGM_REPO/target/release/agents-managerd` | 呼叫 `release-triage-check` 的二進位路徑 |
| `AGM_RELEASE_BOT` | `runtime.json` 的 `release_bot_id`，沒有就 `responder_bot_id` | 派給誰；**不能是巡檢自己**。查不到就跳過，不亂派 |
| `AGM_TRIAGE_QUOTA_MAX` | `85` | 被派的那顆 bot 的 5h 用量 ≥ 這個百分比就不派 |
| `AGM_LOCK_STALE_SECS` | `120` | 鎖沒有可查的執行者時，超過這麼久就當殘留回收 |
| `AGM_LOCK_HUNG_SECS` | `3600` | 執行者還活著但卡了這麼久：推 `ops_alert` 喊人（不搶鎖） |
| `AGM_EXTRA_PATH` | `/opt/homebrew/bin:/usr/local/bin` | 腳本開頭補在 `PATH` 前面的目錄；只給測試蓋掉 |

行為重點：

- **一則交辦最多 5 版／kept＋unmatched 合計 80 條**，順序照 JSON；超過的留給下一輪（JSON 多帶 `deferred_versions`）。第一版一定帶，單版超量也不會永遠卡住。
  request-id 是 `release-triage-<kind>-<to>`；被截斷時 `<to>` 用這批最後一版，下一批才不會撞同一個 id 被 daemon 去重吞掉。
- **額度閘門**：派之前用 `bin/agm state`＋`bin/agm quota` 找被派 bot 的身分那一格（`<kind>:<identity>`，沒有就 `<kind>`），
  5h ≥ `AGM_TRIAGE_QUOTA_MAX` 或有 `limit_hit` → 不派、記一行 log、列維持 `pending`，下一輪再看。查不到（端點壞、找不到那格）**照派**並記 log，
  不因為端點壞了就永遠不做。只在真的有 pending 時才查，一輪只查一次。
- **鎖**（補 #66 留言的洞）：`release-triage.lock` 裡寫 pid＋時間（同 `daemon-update-kick.sh` 的格式）。執行者不在（含 pid 被別的程序重用）就回收接手；
  還活著但超過 `AGM_LOCK_HUNG_SECS` 推 `ops_alert`（`runner_hung`）；回收不掉推 `stale_lock`。
- **依賴**（補 #66 留言的洞）：開頭自補 `PATH=/opt/homebrew/bin:/usr/local/bin:$PATH`；找不到 `python3` 推 `ops_alert`（`missing_dependency`）並寫 log，不靜默 `exit 0`。
  只依賴 `python3` 與 `bin/agm`（額度走 `agm quota`，不用 `curl`／`gh`；開 issue 是 daemon 的事）。
- 某個 kind 的 `release-triage-check` 失敗（抓不到 feed 等）只跳過那個 kind，不當成「沒有新版」，另一個 kind 照跑。

**跟其他 kick 的分工**：

- `claude-release-kick.sh`：看 claude **binary diff**（changelog 沒寫到的東西），**保留不動**；這支看的是 changelog 逐條，兩者互補。
  issue #204 之後的方向是讓 binary diff 由同一支 kick 帶進同一則交辦，那一步不在這次範圍。
- `herdr-update-kick.sh`（#66）：herdr 是另一條管線；#204 說第二階段才把 herdr 併進來，這次不動。#66 留言的兩個洞（殘留鎖、launchd PATH）在這支一次補掉。
- `daemon-update-kick.sh`：鎖回收與 `ops_alert` 的寫法照抄它，格式一致。

launchd plist 範例（`~/Library/LaunchAgents/com.agm.release-triage.plist`；**必須帶 `EnvironmentVariables.PATH`**，launchd 預設 PATH 不含 `/opt/homebrew/bin`）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.release-triage</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/release-triage-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>1800</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

隔離測試：`bash scripts/ops/release-triage-kick_test.sh`（假的 `bin/agm`／`agents-managerd`，含 `env -i PATH=/usr/bin:/bin` 模擬 launchd、殘留鎖與活鎖、額度閘門）；
`release-triage-check` 本身的切條與分桶由 daemon 的 `cargo test` 釘住，不在這裡重測。

正式安裝（**需要 AGM 核准；而且要等 daemon 端的 `release-triage-check` 上線**，沒有那個子命令這支每輪都只會記「檢查失敗」）：

```sh
install -m 755 scripts/ops/release-triage-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/release-triage-task.md ~/.config/agents-manager/supervisor/AGM/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agm.release-triage.plist
```

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
