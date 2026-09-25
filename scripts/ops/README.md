# AGM 運維腳本（scripts/ops/）

正式環境跑的那幾支腳本原本只存在於 `~/.config/agents-manager/supervisor/AGM/bin/`，沒有進版控：
沒有 review、沒有歷史、改壞了也沒得比對。這個目錄是它們在 repo 裡的版本。

**這裡的檔案不會自動安裝。** daemon 只把 `bin/agm`（`scripts/agm.py`）部署到總管 cwd；
這些 kick 腳本要不要裝、什麼時候裝，由 AGM 決定並在有窗口的時候執行。

**裝了哪些、跟 repo 差多少**（issue #418）：要安裝的檔與位置只列在 `install-manifest.tsv`。
`bin/agm ops-sync --check` 唯讀比對安裝端（AGM 目錄）與本機 repo 的 `origin/main`（要最新先 `git fetch`；
`--repo` 預設 `AGM_REPO`，再退回 `~/project/agents-manager`），分四種報：

| 種類 | 意思 |
| --- | --- |
| `drift` | 安裝檔不是 repo 任何一版——有人直接改了安裝檔，最嚴重 |
| `behind` | repo 有更新沒裝；附安裝的是哪個 commit、落後的 commit 標題 |
| `missing` | 對照表有、安裝端沒有 |
| `extra` | `bin/` 裡有、對照表沒有（沒有版控的腳本；`agm` 與 `*.bak*` 不算） |

一致 exit 0；有落差 exit 1，加 `--alert` 另推一則 `ops_alert`（`source=ops-sync`、`reason=installed_out_of_sync`，同一小時一則）。
巡檢每天跑一次 `bin/agm ops-sync --check --alert` 就會被叫醒；它不會替你 install。
`--check` 另外唯讀比對 **`bin/agm`**（issue #532）：`installed` 段是「安裝的不是這顆 binary 內嵌的那份」——加 `--refresh-cli` 就地換掉（`POST /api/supervisor/cli`，不必等 daemon 重啟）；`binary` 段是「binary 內嵌的落後 repo」——那要重建 binary **並重啟 daemon**，這支動不了。daemon 問不到時 `cli.state` 是 `unknown`，不影響 ops 腳本那半邊的結論。

## daemon-update-kick.sh

例行更新：正式 daemon 的 release binary 落後 `origin/main` 時，申請核准、取得 rebuild 租約，
再把重建重啟任務派給建置 child。

### launchd 排程進了版控（issue #487）

`scripts/ops/launchd/com.agm.*.plist` 是這台機器上 8 個 job 的來源檔，`install-manifest.tsv` 也列了它們
（安裝位置寫成 `LaunchAgents/…`，`agm ops-sync --check` 解析成 `~/Library/LaunchAgents/`）。

比對用 `plistlib` parse 過的 dict（**鍵的順序不影響**），**除了 `EnvironmentVariables` 以外全部都比**，
包括 `StandardOutPath`／`StandardErrorPath`／`WorkingDirectory`。`EnvironmentVariables` 不比——裡面是這台
機器的 `PATH` 與 bot id 之類的值，每台不同；repo 那一份留著它是為了 install 之後 job 還跑得起來。

（#487 原本反過來寫成白名單，理由是「launchd 會自己改寫 plist」。**那個理由是錯的**：實機那八份都是純
XML 文字檔，launchd 只讀不回寫；當時看到的「鍵順序不同」是 `PlistBuddy -c Print` 的輸出順序。白名單的
代價是沒列到的鍵被靜默忽略——八份全都有的 `StandardOutPath`／`StandardErrorPath` 就這樣不在比對範圍裡，
而 `browser-gc-kick.sh` 沒有 ops-alert 管道、失敗只留 log，log 路徑漂掉卻報同步就是綠燈假象。issue #499。）

`~/Library/LaunchAgents/` 裡有、對照表沒有的 `com.agm.*` job 會報成 `extra`（＝沒有版控的排程）。
路徑與 `gui/501` 寫死成這台開發機的值，跟 `herdr-full-restart.sh` 同一個處理方式。

**這裡只管版控與比對，不負責 install／`launchctl`**（部署另外走）。

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
approval，所以 approval 表就是唯一真相。數不出來（端點壞了、格式不符）＝未知，**不當 0**（#336）：這輪照常往下檢查、記成失敗，連續 `AGM_FAIL_ALERT_AFTER` 輪推 `ops_alert check_failing`。
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
   真的動到要建的東西，而舊的**已核准、那顆 commit 還沒派過**（在等安全窗口）：照核准的那顆建，不開新申請；「已派過」（`daemon-update.last`）記核准的 commit，HEAD 多出來的留到下一輪另外申請（issue #439：main 約每 5 分鐘一個 push，以前已核准的那張每輪被取代，核准永遠派不出去）。
   舊的還是 pending，或已經 denied／expired／consumed／superseded：新申請帶 `--supersedes <舊 id>`，daemon 把還能用的舊申請標 `superseded`、等待時間接過去（升級計時不因為 main 動了就歸零，SPEC §18.10），不能用的就不接；舊的 `bin/agm` 不認得這個旗標時照舊開新的一筆。pending 換成新 commit 是因為還沒人裁示，讓協調者審的就是現在要建的東西，不必核准後再為 HEAD 多裁示、多建一次。
7. `lease_token` **不進派工正文，也不進 argv**：`mktemp` 建一個 `daemon-update.lease-token.XXXXXX`（權限 600，O_EXCL＋隨機名，不用可預測的固定路徑；上一輪的在拿到新窗口時清掉），自己交還與派工正文都用 `--lease-token-file <路徑>`，由 `agm` 自己去讀（issue #477）。正文會出現在 assignments API、建置 child 的對話紀錄與這份 log；argv 則是同一個 uid 的行程用 `ps` 就看得到，所以 `--lease-token "$(cat …)"` 這種寫法等於把前面的功夫做白工。檔寫不出來時退而用 `--lease-token -`（stdin）。兩處自己交還窗口的 rc **不吞**：失敗時 log 明寫「交還 rebuild 窗口失敗 … 窗口仍被握著」，不會上一行說要交還、下一行就當成還了。token 檔必須是單獨一行，多行直接拒絕（中間的換行會被當成 token 送出去，只換來一句 token 不符）。`--lease-token-file` 要求檔案權限不寬於 600，而且是 open 之後才 fstat、不跟隨 symlink。部署順序：`bin/agm` 要先換成認得這個旗標的版本，舊的 `bin/agm` 會以 rc 2 退掉。
5. `daemon-update.lock` 防止腳本重疊執行，鎖裡寫 pid 與時間。執行者已經不在（強制關機、斷電、SIGKILL）就**自己回收**並接手這一輪；
   還活著但卡超過 `AGM_LOCK_HUNG_SECS`（預設 3600 秒）不搶它的鎖，改推一則 `ops_alert` 給 AGM。核准狀態檔損毀、或核准 ID 查不到
   （先用 `approval list --id` 查，清單只回最新 100 筆）一樣停住並推 `ops_alert`，不自動繞過——以前這些只寫 log 就 `exit 0`，
   換版流程永久、靜默地停住（review3 c1 M1）。
6. AGM 雙角色（SPEC §18.15）：idle 檢查另外排除協調者（`agm responder show` 有 `bot_id` 時）；runtime.json 有 `role` 才帶 `assign --review-by <role>`
   （巡檢目錄＝`patrol`，更新結果回巡檢驗收）。舊部署兩者都沒有，行為照舊。

### 立即部署（使用者 2026-09-25，SPEC §18.2）

網頁左上角按「立即部署」時，daemon（`POST /api/deploy/now`）以使用者的名義核准一筆 rebuild（requester＝`daemon-update-kick`），
寫 `daemon-update.now.json`（`{approval_id,sha,live_sha,requested_at,requested_by}`），再 `launchctl kickstart gui/<uid>/com.agm.daemon-update`。
kick 讀到這個檔就走立即模式：

- **略過**：觸發條件、`daemon-update.last`（同一顆已派過）、申請／等 AGM 裁示 rebuild。
- **核對那筆核准**：`approval list --id`，要 `purpose=rebuild`、`requester` 是自己的 `OWNER`、`target_commit` 等於請求的 sha、`approved` 沒過期。查不到＝這輪不知道（留著請求）；不能用＝`ops_alert now_approval_unusable` 並收掉請求。
- **建確認框上那顆**：sha 要是 origin/main 的祖先（否則 `ops_alert now_target_invalid`）；`.built` 到它之間沒有程式碼差異就收掉請求（已經是最新）。
- **照舊**：建置 child 要在、上一筆更新要結案、`lease safety`／`acquire`（等不到窗口就留著請求，下一輪再試）、token 檔、派工正文的固定條件。
- **restart**：拿到 rebuild 窗口後以 `AGM_BUILD_BOT` 申請 restart（`--request-id deploy-now-restart-<rebuild 核准>`），daemon 在建立當下核准（kick 不打 decide，#447），回應是 `approved` 才叫 child 直接用它；開不出來就寫明「照 3c 自己申請」。
- **派工**：request id `agm-daemon-update-<sha>-now-<核准>`，成功才刪請求檔；`daemon-update.approval.json` 改指那一筆。請求檔壞掉 → `ops_alert now_request_corrupt`、刪掉，這輪回到例行判斷。

daemon 的 `kick_ready` 以「裝好的 kick 裡有沒有 `daemon-update.now.json` 這個字」判斷，所以**要先 install 這一版 kick，按鈕才按得下去**（舊 kick 不會讀請求檔，按了只會永遠停在「部署中」）。

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

#### 破壞性指令的護欄（issue #422，必讀）

`*_test.sh` 會把被測腳本複製出來真的執行，而那些腳本裡有 `launchctl bootout`、`pkill -f`、`ssh`。
2026-09-23 17:39Z 一支測試的 PATH 沒擋住，真的把 `gui/501` 的兩個 herdr launchd job bootout 掉，
全機 pane 當場消失。所以 `scripts/check.sh ops` 會把每一支測試包在
`scripts/ops/destructive-canary.sh` 底下跑，三層攔截：

1. **PATH**：把 canary 目錄放在最前面。便宜，但**腳本自己整個覆寫 PATH 就失效**。
2. **匯出的 bash 函式**：函式優先於 PATH 查找，覆寫 PATH 蓋不掉，而且跟著環境進到子 bash。
   `export -f` 是 bash 專有的——在 zsh 底下它會變成「印出定義」，看起來成功但根本沒匯出。
3. **zsh 的 `$ZDOTDIR/.zshenv`**：zsh 對每一個 shell（含 `zsh -c`、含巢狀）都會讀它，
   跟 PATH 無關。沒有這層的話，子 zsh 在覆寫 PATH 之後完全沒有保護。

`rm` 不整支攔（測試本來就要清自己的暫存目錄），只擋**絕對路徑且在暫存目錄之外**的目標——
`rm -rf ~/foo` 展開後正好是那種。要放行別的根目錄就設 `AM_CANARY_ALLOW_RM`。

**護欄擋不到的（實測過，不要以為有三層就安全）**：

- **絕對路徑呼叫**（`/bin/launchctl`）：不經過 PATH，也不經過 shell 的函式查找，三層都抓不到。
  所以**不要用絕對路徑叫破壞性指令**；要指定就用 `LAUNCHCTL_BIN=` 這種間接變數指到替身
  （`daemon-swap_test.sh` 是這樣做的）。
- **非 shell 的子行程**（python 的 `subprocess`、`Bun.spawn`、任何走 `execvp` 的）：
  第 2、3 層保護的是**shell 的指令查找**，`execvp` 根本不經過 shell，所以對它們**只剩 PATH 那一層**。
  實測：PATH 完整時擋得住，PATH 被覆寫時 python／Bun 都直接穿過去（`FileNotFoundError`／
  `Executable not found in $PATH`，而那只是因為用的是 sentinel；換成真指令就會執行）。
  ⇒ **測試呼叫 `.py`／`.ts` 時，那一行的 PATH 一定要帶上 `${AM_CANARY_DIR:+$AM_CANARY_DIR:}`**。
  `lint` 會掃「直譯器＋腳本在同一行」的寫法；用變數叫的（`"${BUN}" "${SCRIPT}"`）掃不出來，靠這條規則自律。
  更深一層——測試叫一支 `.py`、那支自己再 spawn——`lint` 完全看不到，寫那種測試要自己確認 PATH。
- **`env -i`**：把匯出的函式與 `ZDOTDIR` 一起清掉，只剩你自己在那一行給的 PATH。

`rm` 護欄的界線：目標會先正規化成絕對路徑、並解開路徑中的 symlink 再比對，所以
`cd /tmp && rm ../Users/…` 與「`/tmp` 底下指到暫存外的 symlink」都擋得住。
**但正規化只解開目錄部分**：最後一段本身是 symlink 時比的是連結自己（刪連結不刪目標，這是對的）。

寫新測試時：

- PATH 要帶上 canary：`PATH="$你的fakebin:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin"`。
- 真的需要最小環境（模擬 launchd 之類）就在那一行上面寫
  `# canary-gap: <理由，以及那段裡的破壞性指令是怎麼另外擋住的>`，`lint` 就會放行。
  重點是**有人寫下理由**，不是默默漏掉。
- `scripts/ops/canary-baseline.tsv` 是棘輪：既有的漏洞記在裡面，只能變少。
  修好之後跑 `scripts/ops/destructive-canary.sh lint --update` 把數字縮小。

**驗證護欄本身時，只准用 `am-canary-probe` 這個無害的 sentinel 指令名**，
絕對不要拿真的 `launchctl`／`pkill` 當白老鼠：護欄沒生效時，白老鼠就變成真的破壞——
#422 的事故正是這樣來的。

### 正式安裝（需要 AGM 核准）

```sh
install -m 755 scripts/ops/daemon-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/daemon-update-task.md ~/.config/agents-manager/supervisor/AGM/
```

任務內容在 `daemon-update-task.md`（kick 會把它當成派工正文的開頭，末尾再補這一輪的 sha／核准／租約）。
這份是**來源檔**：改規則改這裡再 install，不要只改 AGM 目錄裡那份，否則下次有人從 repo 安裝就把規則改回去了。

launchd：`com.agm.daemon-update` 改成每 5 分鐘跑一次（`StartInterval 300`），由腳本自己判斷
「整點、集滿門檻，或等太久」；`AGM_BUILD_BOT`（必要）與 `AGM_REBUILD_THRESHOLD`／`AGM_REBUILD_MAX_WAIT_MIN`（可選）放 `EnvironmentVariables`。

## daemon-swap.sh（＋ daemon-start.py）

建置 child 換 binary 用的那一段：拿 restart 窗口 → 備份 DB → 換 binary → 重啟 → 驗證 → 寫 `.built`。
以前每趟由 child 在 scratchpad 臨時寫一份，2026-09-20 就因為把 `user_version` 寫死成 10（那批升到 11）
誤判成失敗、回滾、舊 binary 被版本閘擋下，daemon 停了 33 秒。所以它進了版控，行為由
`daemon-swap_test.sh` 釘住：

- 預期 schema 版本從 checkout 的 `SCHEMA_HISTORY` 讀，不寫死；讀不到就中止。
- 回滾還原 DB 前先停 daemon、清掉 `-wal`／`-shm`，還原後自驗 `user_version` 與 `integrity_check`。
- 升過 schema 的失敗**預設往前修**（沿用新 binary，exit 6），只有新 binary 起不來才還原 binary＋DB（exit 7）。
- 啟動走 `launchctl submit` ＋ `daemon-start.py`（fork + setsid）：daemon 是 ppid=1、nice 0。
  在 pane 裡直接背景起會繼承 pane 忙碌時的 nice 5，非 root 降不回去。
- 重啟後比對 bot 名單（看 id）：少了就回滾，**只有**換版窗口內刻意刪掉的不算——deleted_at 在窗口起點之後、
  而且有刪除 API 留下的 `delete_bot`／`delete_project` intent（subject 是它、它的專案，或 payload 快照裡有它），DB 唯讀查。
  只有 deleted_at、沒有 intent（重啟後 reconcile 退役、投影軟刪）照樣回滾（issue #553：2026-09-24 父 bot 在窗口內刪 child i263 被誤判回滾）。
  父 bot 用 `herdr pane close` 收 child（不呼叫 DELETE）時，daemon 退役那顆會寫 `retire_child` 紀錄；只認 subject 是它、窗口內、
  `cause` 是 `pane_closed`（herdr 報過關閉事件、當下 pane 也不在）或 `promoted` 的。`agent_missing`／`unconfirmed`／`herdr_restarted`
  是換版會弄丟 child 的樣子，照樣回滾（issue #554，判準見 SPEC §6.5a）。

```sh
scripts/ops/daemon-swap.sh --sha <完整 sha> --old <short sha> --old-hash <sha256 前 16 碼> \
    --approval <restart 核准 id> --owner <自己的 bot id> --checkout <乾淨 worktree>
```

離開碼：0 成功、2 參數錯、3 前置核對失敗、4 沒窗口／複查不安全、5 備份有問題、6 往前修後停在新 binary、7 已回滾、8 換版成功但 restart 窗口沒交還成功（issue #477）。**窗口不會自己消失**：它要撐到租約的 `expires_at`——預設 900 秒（`maintenance::DEFAULT_TTL_SECS`，上限 3600，且不會晚於那張核准的到期時間），這段時間內 supervisor 的 assignment 派送是停的、也沒有人拿得到 restart 窗口。看到 8 就是要有人處理：等 TTL 到期，或請 AGM 用 `lease release restart --force` 附理由接管，不要當成換版順利結束。

`lease_token` **不進 argv**（issue #477）：拿到窗口之後 `mktemp` 在 AGM 私有目錄底下建一個 0600 的檔（不可預測路徑、不落在全域可寫的 /tmp），`agm lease release` 走 `--lease-token-file` 讀，腳本結束時（不管成敗）刪掉。argv 對同一個 uid 的行程是公開的（`ps`），而那顆 token 是「只出現一次、任何 API 都查不到」的一次性憑證，抄走就能收掉別人正在換 binary 的窗口。交還的 rc 也不再被 `>/dev/null 2>&1` 吞掉——以前失敗時 log 照樣寫「窗口已交還」，而窗口其實握到 TTL。

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
install -m 644 scripts/ops/codex-release-task.md ~/.config/agents-manager/supervisor/AGM/
```

`codex-release-task.md` 沒有對應的 kick：只給網頁更新框的「請 AGM 解析」（`POST /api/claude-update/review {kind:"codex"}`，issue #561）用，
沒裝進 AGM 目錄時 daemon 退回讀 repo 的 `scripts/ops/`。

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
| `AGM_EXTRA_PATH` | `~/.local/bin:/opt/homebrew/bin:/usr/local/bin` | 腳本開頭補在 `PATH` 前面的目錄（順序照登入 shell 的 `which herdr`）；空字串＝不補；只給測試蓋掉 |
| `AGM_LOCK_STALE_SECS` | `120` | 鎖沒有可查的執行者（含舊版腳本留下、沒有 owner 檔的鎖）時，超過這麼久就當殘留回收 |
| `AGM_LOCK_HUNG_SECS` | `3600` | 執行者還活著但卡了這麼久：推 `ops_alert`（`runner_hung`），不搶鎖 |
| `AGM_FAIL_ALERT_AFTER` | `2` | 連續幾輪沒能完成檢查就推 `ops_alert`（`check_failing`） |

狀態檔 `herdr-update.last`＝已經派過工的版本，派工成功才寫；同一版不重派。
開頭自補 PATH（launchd 預設不含 Homebrew，herdr／gh 多半在那裡）；找不到 `herdr`／`gh`／`python3`／`curl` 推 `ops_alert`（`missing_dependency`）並寫 log，不靜默 `exit 0`。
**鎖**（#66 review 留言）：`herdr-update.lock` 裡寫 pid＋時間（同 `release-triage-kick.sh`）。SIGKILL／斷電讓 EXIT trap 沒跑、鎖留在磁碟上時，
下一輪發現執行者不在（含 pid 被別的程序重用）就回收接手並記一行 log；舊版純 `mkdir` 鎖（沒有 owner 檔）超過 `AGM_LOCK_STALE_SECS` 一樣回收，
剛建立的先不動；執行者還活著但超過 `AGM_LOCK_HUNG_SECS` 推 `runner_hung`，回收不掉推 `stale_lock`。
**連續失敗要被看見**（#66 review 留言）：讀不到本機版本、查不到最新版、抓不到 CHANGELOG、版本比較失敗、比較報告看不懂（不是 JSON、沒有布林的 `should_notify`，#226——不當成「沒有新版」）、找不到派給誰、派工失敗、binary 不在，這些「這輪沒能完成檢查」的出口
除了 log 還會在 `herdr-update.fails` 記連續次數；連續 `AGM_FAIL_ALERT_AFTER` 輪（每天一輪＝隔天還是不行）推 `ops_alert`（`check_failing`），一次網路抖動不吵人。
檢查完整跑完（含「沒有新版」）或派工成功就清零。
隔離測試：`bash scripts/ops/herdr-update-kick_test.sh`（假的 `herdr`／`gh`／`agents-managerd`／`bin/agm`，`file://` CHANGELOG；含 `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin` 模擬 launchd、缺依賴喊人、殘留鎖與活鎖、連續失敗喊人，系統 `/bin/bash` 3.2 也跑）；
`herdr-update-check` 本身的版本比較與 CHANGELOG 段落擷取正確性由 `cargo test -p agents-managerd` 釘住
（`daemon/src/changelog.rs`、`daemon/src/herdr_update.rs`），不在這支腳本測試裡重測。

安裝（**需要 AGM 核准**；不要自己 `launchctl bootstrap`）：

```sh
install -m 755 scripts/ops/herdr-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 755 scripts/ops/herdr-lan-check.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

升級核准後的完整人工步驟見 [`herdr-upgrade-runbook.md`](herdr-upgrade-runbook.md)。其中
`herdr-lan-check.sh` 必須從 herdr pane 執行，**第一個參數寫真 binary 的路徑**（`/opt/homebrew/bin/herdr`）：不帶參數時它會跳過 PATH 上的 per-bot shim（`~/.config/agents-manager/bots/*/bin/herdr`，沒簽章）與 shell 腳本、解開 symlink，但 PATH 上只有 shim 時就是明確 FAIL。它用新 binary 的 `codesign -dv` identifier 查
`/Library/Preferences/com.apple.networkextension.plist`，再用 node 連區網；Apple 內建 nc、python3、curl
不能代驗。任何失敗或 node 缺少都停下，請使用者授權「本機網路」，不要靠重啟硬試。

launchd plist 範例（`~/Library/LaunchAgents/com.agm.herdr-update.plist`；**必須帶 `EnvironmentVariables.PATH`**，launchd 預設 PATH 不含 `/opt/homebrew/bin`，herdr／gh 在那裡，herdr 也可能在 `~/.local/bin`；腳本開頭另外自補一次）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.herdr-update</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/herdr-update-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>86400</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/Users/USER/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

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
- **派成功要寫回帳本**：`assign` 成功後對**這一則實際帶出去的版本**（截斷後那批，不是全部 pending）呼叫
  `bin/agm release-triage dispatched --kind <k> --version <v>…`，帳本才會離開 `pending`（下一輪不再回同一批、「`dispatched` 超過 6 小時沒 verdict 退回 pending」的計時才會開始）。
  `assign` 失敗不標；`dispatched` 本身失敗只記一行 log、照舊 `exit 0`，**不重派**（request-id 會擋住重複交辦）。
- **`publish` 重試**：每輪（不論有沒有 pending、額度擋不擋）對兩個 kind 各呼叫一次 `bin/agm release-triage publish --kind <k>`，
  給 gh 失敗停在 `judged` 的版本一個重試入口（daemon 端不做定時器）。沒東西要重試（results 空、`disabled`、`deferred`）時安靜；只有 `published`／`failed` 才寫 log。
  舊的 `bin/agm` 不認得 `release-triage`（argparse exit 2）：log 講一次就略過，用 `release-triage-publish-unsupported` 這個 state 檔記「已經講過」，換新 agm 後自動清掉。
- 某個 kind 的 `release-triage-check` 失敗（抓不到 feed 等）只跳過那個 kind，不當成「沒有新版」，另一個 kind 照跑。

**跟其他 kick 的分工**：

- `claude-release-kick.sh`：看 claude **binary diff**（changelog 沒寫到的東西），**保留不動**；這支看的是 changelog 逐條，兩者互補。
  issue #204 之後的方向是讓 binary diff 由同一支 kick 帶進同一則交辦，那一步不在這次範圍。
- `herdr-update-kick.sh`（#66）：herdr 是另一條管線；#204 說第二階段才把 herdr 併進來，這次不動。#66 留言的兩個洞（殘留鎖、launchd PATH）在這支一次補掉；`herdr-update-kick.sh` 之後也照同一套補上（鎖、PATH、缺依賴與連續失敗喊人）。
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

**打開 `[release_triage] publish = true` 之前**（#204 的 close condition）：先乾跑一次，看它會開哪幾張、
內文長什麼樣、去重會不會命中、標籤齊不齊。乾跑只讀 gh（`auth status`／`repo view`／`label list`／`issue list`），
**一張 issue 都不開、帳本一個字都不寫**，所以在 `publish = false` 的現況下就能跑：

```sh
~/.config/agents-manager/supervisor/AGM/bin/agm release-triage publish --dry-run          # 全部 judged 的版本
~/.config/agents-manager/supervisor/AGM/bin/agm release-triage publish --dry-run --kind codex --version 0.156.0
```

`checks` 要全綠才會真的開得出 issue：`gh_auth_ok`、`repo_ok`、`can_write`（`viewer_permission` 是
`ADMIN`／`MAINTAIN`／`WRITE`／`TRIAGE`）、`issues_enabled`、`labels_missing` 是空的——
`gh issue create --label` 對**不存在的標籤是硬失敗**，少一個就會整版停在 `judged`，
所以 `release-triage`／`upstream:claude`／`upstream:codex`／`triage:guard`／`triage:adopt` 這五個要先在 repo 上建好。
每個提案的 `action` 是 `create`（會開）｜`comment`（`duplicate_of`，只留言）｜`existing`（遠端已有同標記，含已關的，不會重開）｜
`already_logged`｜`skipped_version_limit`｜`deferred_daily_limit`｜`remote_unknown`（gh 檢查沒過，去重問不到）。
`publish = true` 之後 kick 每輪的 `publish` 重試會把 `judged` 的版本一次開出來（每版 ≤4 張、24 小時 ≤8 張），
所以打開前先確認 `would_create` 的數字是預期的。

## ci-watch-kick.sh

main 的 GitHub CI 盯哨（issue #211）。2026-09-16 起 main 的 CI 連紅好幾天沒人發現——規則只要求跑本機 `check.sh`，沒人看 GitHub 的結果。
launchd `com.agm.ci-watch` 每 10 分鐘跑一次；只開 issue、留言、派工，**不改程式、不重啟、不關 issue**。形狀比照 `release-triage-kick.sh`（鎖與殘留回收、開頭自補 PATH、缺依賴走 `ops-alert`、無事不寫 log）。

- 每輪 `gh run list --branch main --workflow CI -L 20`，只看**已完成**的 run：success 算綠，failure／timed_out／startup_failure 算紅，cancelled／skipped／進行中當沒看到。
- 狀態檔 `ci-watch.state.json` 記目前這一段紅：`first_red_sha`、`first_red_run`、`issue`、`assigned`、`failures`。
  - **綠→紅**：從 `gh run view <id> --log-failed` 抽失敗的測試名（cargo 的 `test X ... FAILED`／`failures:` 區塊、python 的 `ERROR:`／`FAIL:`），開一張 issue（標題 `CI 紅了：<第一個紅的 sha 前 8 碼> 起 N 條失敗`，標籤 `ci-red`，不存在就建；內文有第一個紅的 run、失敗清單、上一個綠到第一個紅之間的 `git log`），再 `bin/agm assign … --request-id ci-red-<first_red_sha>` 派工。開 issue 前先看有沒有開著的 `ci-red` issue（狀態檔遺失時接手它，不重開、不重派）。
  - **還在紅**：不重開、不重派；失敗清單多了新的測試才在同一張 issue 留言一次。上一輪派工失敗會補派（同 request-id，daemon 去重）。
  - **紅→綠**：在 issue 留言「<sha> 起恢復綠，run <id>」，清狀態；**不自動關 issue**，由修的人關。
- `gh` 失敗／rate limit：這輪什麼都不做、不改狀態、記一行 log，不誤報紅或綠。
- 派給誰：`AGM_CI_BOT` ＞ `runtime.json` 的 `ci_bot_id` ＞ `responder_bot_id`；不能是巡檢自己。交辦正文在 `ci-watch-task.md`。
- 一段紅沒人關 issue 就恢復綠、下一段紅又來時，會接手那張還開著的 issue；所以修的人要記得關。

launchd plist 範例（`~/Library/LaunchAgents/com.agm.ci-watch.plist`；**必須帶 `EnvironmentVariables.PATH`**，launchd 預設 PATH 不含 `/opt/homebrew/bin`，`gh` 在那裡）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.ci-watch</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/ci-watch-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>600</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

隔離測試：`bash scripts/ops/ci-watch-kick_test.sh`（假 `gh`、假 `bin/agm`，含 `env -i` 最小 PATH、殘留鎖與活鎖、gh 失敗、狀態檔遺失）。不會真的呼叫 `gh issue create`。

正式安裝（**需要 AGM 核准**）：

```sh
install -m 755 scripts/ops/ci-watch-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/ci-watch-task.md ~/.config/agents-manager/supervisor/AGM/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agm.ci-watch.plist
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

## outbox-gc.sh／pane-gc.sh／browser-gc-kick.sh（issue #318）

三支純機械的清理腳本，原本只存在 AGM 的 `bin/`，沒有版本控制也沒有測試，卻都是破壞性的（刪檔、殺行程、關 pane）。
現在 repo 是來源，內容與已安裝的那份逐位元組相同（`cmp`），另附 `browser-gc-task.md`（browser-gc bot 的交辦正文）。

- `outbox-gc.sh`：launchd `com.agm.outbox-gc` 每 10 分鐘；刪 outbox 底下（含 bot 子目錄）超過 60 分鐘的檔與空目錄。護欄：`AM_OUTBOX_ROOT` 不在 `~/.config/agents-manager/outbox*` 就拒絕。
- `pane-gc.sh`：關卡住超過 24 小時的 `claude auth login`／`gcloud auth login`／`codex login` pane；幽靈 pane 只記錄。由 `browser-gc-kick.sh` 呼叫。
- `browser-gc-kick.sh`：launchd `com.agm.browser-gc`，**`StartInterval 1800`（30 分鐘）**；收孤兒、無 CDP 連線、活超過 2 分鐘的 headless Chrome，刪沒人用的 `/tmp/am-*` Chrome profile，跑 `pane-gc.sh`，再派 `browser-gc-task.md`。有鎖與殘留回收（issue #490）。
  （這裡原本寫「每 6 小時」，但實機一直是 1800 秒——是**文件寫錯**，不是排程跑錯；issue #487 把 plist 收進版控時照實機現值定案。）

隔離測試：`bash scripts/ops/outbox-gc_test.sh`、`pane-gc_test.sh`、`browser-gc-kick_test.sh`（假 `ps`／`lsof`／`herdr`／`bin/agm`，`kill` 用函式替身，`/tmp/am-*` 換成暫存目錄；不會殺行程、關 pane 或碰真的 outbox／HOME）。
安裝比照其他 kick：`install -m 755 scripts/ops/{outbox-gc,pane-gc,browser-gc-kick}.sh ~/.config/agents-manager/supervisor/AGM/bin/`、`install -m 644 scripts/ops/browser-gc-task.md ~/.config/agents-manager/supervisor/AGM/`；launchd 由巡檢處理。

## dev-server-kick.ts（issue #418）

5173 dev server 的看門狗。**2026-09-24 之前只存在於 `AGM/bin/`**：`docs/SPEC.md` §18.1 把行為寫得很細，
程式碼卻沒有版控、沒有 review、沒有測試，而且它會 `kill` 占用 port 的行程。現在 repo 是來源檔。

- launchd `com.agm.dev-server`：`StartInterval 60`、`RunAtLoad`，跑 `bun run …/AGM/bin/dev-server-kick.ts`。
- 行為與判斷順序見 `docs/SPEC.md` §18.1（健康＝綁 `*`／`0.0.0.0` 且本機 curl 有回應；只收孤兒 vite；
  找不到 node 或 `vite.js` 寧可這輪不起，也不拿 bun 代跑）。
- **看門狗用 bun、vite 一律用 node**：bun 的 upgrade socket 沒有 `destroySoon`，daemon 一重啟代理斷線 vite 會 crash。

隔離測試：`bash scripts/ops/dev-server-kick_test.sh`（假 `lsof`／`ps`／`git`／`bun`／`node`，副本的 `PORT` 換成
測試 port，假 `lsof` 只回報測試自己 spawn 的 pid；**不會碰真的 5173 或真的 vite**）。沒有 bun／python3 會自己 skip。

安裝（**需要 AGM 核准**）：

```sh
install -m 755 scripts/ops/dev-server-kick.ts ~/.config/agents-manager/supervisor/AGM/bin/
```

## herdr-full-restart.sh（issue #418）

herdr 全機重啟：bootout 兩個 herdr launchd job → 殺掉所有 herdr server → 清 socket → bootstrap 回來 →
補起預設 server。2026-09-22 使用者下令全機重啟時寫的，同樣原本只在 `AGM/bin/`。**沒有 launchd 排程**，手動跑。

- 順序是關鍵：**先 bootout 再殺 server**，反過來 launchd 會立刻把 server 拉回來。
- **起 default server 這一步是唯一沒有自癒路徑的**：被殺掉的 session 裡，`agents-manager` 與
  `am-attach-remote` 有 launchd job 會 bootstrap 回來，其他具名 session 由 daemon 的
  `ensure_session`（`daemon/src/state.rs`）在下次要用時自動重啟；**只有 `default` 不會**——
  `ensure_session` 明文拒絕代起使用者自己的 session。所以那一步起不來就要讓人知道。
- 修過的（issue #455，2026-09-24）：以前是 `nohup setsid …`，而 macOS 根本沒有 `setsid`
  （util-linux 才有），所以 `nohup` 找不到它直接失敗、herdr 一次都沒被執行；更糟的是那行
  寫成 `cd … && nohup … &`，`&` 綁的是整個 `&&` 清單，`$!` 拿到的是 subshell 的 pid，
  **起不起得來都有值**，於是失敗被記成「default server started pid=…」。2026-09-22 的 log 裡
  `session list` 顯示 `default stopped` 就是這個。現在：不用 `setsid`（`nohup` + `&` + `disown`
  就夠，跟 `ensure_session` 同一款），`$!` 取的是 server 本身，而且要 `herdr session list` 看到
  `default running` 才記 `default server up`，等不到就寫 FAIL 並以 rc=1 結束。
- 路徑與 uid（`gui/501`）寫死成這台開發機的值，是收進 repo 時保留的既有行為。

隔離測試：`bash scripts/ops/herdr-full-restart_test.sh`（`launchctl`／`pkill`／`pgrep`／`sleep`／`herdr`
全部用**注入的 shell 函式**攔截，不靠 PATH；socket 與 plist 都在暫存目錄）。

安裝（**需要 AGM 核准**）：

```sh
install -m 755 scripts/ops/herdr-full-restart.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

## 已安裝版與 repo 的落差（issue #418 稽核，2026-09-24）

這些檔沒有自動同步，所以會漂。2026-09-24 的逐支比對（`git hash-object` 對 `origin/main` 的 blob）結論：
同名檔沒有任何一支「安裝版比 repo 新」；`browser-gc-kick.sh` 與 `pane-gc.sh` 落後一個純寫法的 commit。

- **`herdr-lan-check.sh` 目前沒有裝**，但上面 herdr-update-kick 那節寫著要 `install`。
  實際用法（`herdr-upgrade-runbook.md`、`herdr-update-kick.sh` 的派工正文）都是從 repo checkout 跑
  `bash scripts/ops/herdr-lan-check.sh /opt/homebrew/bin/herdr`，所以沒裝不影響升級流程；
  要嘛補裝、要嘛把那行 install 拿掉，兩邊挑一個對齊。
- `daemon-swap.sh`、`daemon-start.py`、`herdr-upgrade-runbook.md` 本來就不裝（從 checkout 跑），不是缺口。
