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
| `behind` | repo 有更新沒裝；附安裝的是哪個 commit、落後的 commit 標題（腳本比位元組、plist／unit 比語意，**兩種都回頭找歷史**：安裝的是 repo 以前某一版就是 `behind`，不是 `drift`） |
| `missing` | 對照表有、安裝端沒有 |
| `extra` | `bin/` 裡有、對照表沒有（沒有版控的腳本；`agm` 與 `*.bak*` 不算） |

對照表可選的第三欄 `darwin`／`linux` 表示只在那個平台裝（省略＝兩邊都裝）。另一個平台的列放在報告的
`skipped`、不算落差；`platform` 欄寫這次是用哪一邊判斷的。排程列一定要標對（`LaunchAgents/…` 標 `darwin`、
`systemd/…` 標 `linux`），標錯是 `bad_manifest`（exit 2），不會變成一堆 `missing`。

一致 exit 0；有落差 exit 1，加 `--alert` 另推一則 `ops_alert`（`source=ops-sync`、`reason=installed_out_of_sync`，同一小時一則）。
agm-host 上由 `ubuntu-ci.sh` 排程它（每 `AGM_CI_OPS_SYNC_INTERVAL` 秒，預設 6 小時，不靠新 commit 觸發；`--alert` 會叫醒巡檢，結果留在 `~/.cache/agents-manager/ci/ops-sync.json`）——
以前只有文件寫「巡檢每天跑一次」、沒有任何東西在排程，`outbox-gc.sh` 停在 9/28 的舊版也沒人發現。它只偵測，不會替你 install。
`--check` 另外唯讀比對 **`bin/agm`**（issue #532）：`installed` 段是「安裝的不是這顆 binary 內嵌的那份」——加 `--refresh-cli` 就地換掉（`POST /api/supervisor/cli`，不必等 daemon 重啟）；`binary` 段是「binary 內嵌的落後 repo」——那要重建 binary **並重啟 daemon**，這支動不了。daemon 問不到時 `cli.state` 是 `unknown`，不影響 ops 腳本那半邊的結論。

## 已安裝 ops 腳本的自動換新（`ops-install.sh`，旗標，預設關）

根因（2026-10-01，`outbox-gc.sh` 停在 9/28 的舊版）：安裝一直是手動的，換版流程只管 daemon binary——而 ops 腳本的改動根本不會
產生新 binary（`daemon-update-kick.sh` 看到「沒有會進 binary 的差異」就收工），所以沒有任何一步會把它們裝過去。

`ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run]`：

- 只看 `install-manifest.tsv`（`--ref` 那版）這個平台的列，來源用 `git show <ref>:<path>` 讀。
- **只更新「已經裝了、內容跟 repo 不同」的檔**。安裝端沒有的（新檔）報 `not-installed`，第一次要手動裝；
  排程 unit（`systemd/`、`LaunchAgents/`）報 `skipped`、不碰（換了還要 daemon-reload／launchctl）；清單外的檔不碰。
- 每支先把新版寫到同目錄暫存檔、**在暫存檔上自檢**（`.sh` → `bash -n`、`.py` → 語法編譯、`.ts` → 有 bun 就 `bun build`）；
  沒過就丟掉暫存檔、報 `failed`、安裝位置的舊檔一個位元都沒動（不會讓排程撞到沒驗過的新版）、其他支照裝。
  過了才備份到 `<AGM>/ops-install-backups/<UTC 時間>/<安裝位置>`，再 `mv`（原子替換）。
- 被 SIGKILL 的上一次會把暫存檔 `<安裝位置>.new.<pid>` 留在安裝端：下次執行（非 `--dry-run`、拿到鎖之後）把清單內、`.new.<純數字>`、pid 已不在的清掉並印 `cleaned N`（pid 還活著的不碰）；`ops-sync --check` 也不把這種檔名報成 `extra`。
- 同時只能有一個在裝：`<AGM>/ops-install.lock.guard` 的 OS advisory lock 串行化鎖目錄建立與殘留回收；`<AGM>/ops-install.lock/` 記 pid，SIGKILL 後可接手。拿不到鎖 exit 3、不動任何檔。同一秒內重複裝，備份目錄也不共用（`<UTC 時間>-2`…）。
  安裝位置是 symlink 的報 `skipped`（`mv` 會把連結換成拷貝）；父目錄是 symlink、或對照表的安裝位置跑出 `--dir`（絕對路徑、`..`）都報 `failed`、不寫，也不會沿父 symlink 清孤兒暫存檔。
- **手改過的不覆蓋**：安裝端的檔不是 repo 任何一版（`git log <ref> -- <來源>` 的 blob 都對不上，跟 `ops-sync --check` 的 `drift` 同一條），或自檢／備份期間目的地內容改變，報 `drifted`、不動它、不算失敗；kick 會另推 `ops_install_drift` 叫人看。手動 `--force` 才換（舊檔照樣備份）。
- `--dry-run` 只列 `would-install`。最後一行 `changes=N failed=M drifted=K`；有失敗 exit 1。整輪成功（而且真的換了檔）會把「時間 commit」寫進 `<AGM>/ops-install.last`；有失敗的那輪**不更新**它，改把「時間 commit failed=N」記進 `ops-install.last-failed`（下一次整輪成功就刪）。這兩個檔目前沒有程式讀（kick、ops-sync 都不看），只給人查「裝到哪一版了」。

**接到部署**：`daemon-update-kick.sh` 在旗標開著時，於 ① 換版成功之後、② 「沒有會進 binary 的差異」那一輪（要該 sha 的 `ubuntu-ci` 綠燈）
用該 sha 自己的 `ops-install.sh` 換新；失敗推 `ops_alert`（`ops_install_failed`），不影響部署結果。**旗標預設關**，要由使用者或 AGM 開：
`touch ~/.config/agents-manager/supervisor/AGM/ops-auto-install.enabled`（或給 kick 的環境 `AGM_OPS_AUTO_INSTALL=1`），關掉就刪檔。
注意：kick 本身也是已安裝的檔，這一版 kick 要先手動 `install` 一次，旗標才有東西可開。

## daemon-update-kick.sh

例行自動部署（使用者 2026-09-29 簡化）：正式 daemon 的 release binary 落後最新一顆 `ubuntu-ci` 綠燈的 `origin/main` 時，
**腳本自己直接**建置並換版——不經 LLM、不開核准單、不派建置 child。建置已在推 main 前測過、`ubuntu-ci` 也在背景跑整樹，AGM 不再驗一次。
流程（SPEC §18.2）：`git fetch`（專用 checkout）→ 跟 `daemon-update.built` 比，沒有會進 binary 的差異就結束 →
沿 first-parent 往回找最新一顆 `ubuntu-ci`＝success 的 sha（`gh api repos/Eden-Sun/agents-manager/commits/<sha>/status`）→
專用 checkout 上 `bun run build`＋`cargo build --release -p agents-managerd` → `daemon-swap.sh` 換版。

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

### Linux 主機：systemd user unit（issue #677）

搬到 Linux 主機（#675）後，同一批 job 由 systemd user manager 排程：`scripts/ops/systemd/com.agm.<名字>.service`
（`Type=oneshot`，跑同一支安裝好的 kick）＋`.timer`，對照表以 `systemd/…` 列出（解析成 `~/.config/systemd/user/`，
有 `XDG_CONFIG_HOME` 就跟著它）。對應規則，`scripts/agm_test.py` 的 `SystemdParityTest` 釘住、改一邊忘了另一邊會紅：

| launchd | systemd |
| --- | --- |
| `ProgramArguments` | `ExecStart=`（直譯器同名，`/Users/<人>/` 寫成 `%h/`；bun 在 Linux 是 `%h/.bun/bin/bun`） |
| `StartInterval N` | `.timer` 的 `OnUnitActiveSec=Ns`，第一次 `OnActiveSec=Ns`（launchd 也是載入後隔一個間隔才第一次） |
| `RunAtLoad` | `OnActiveSec=1s`（timer 一啟動就跑一次） |
| `StandardOutPath`／`StandardErrorPath` | `StandardOutput=`／`StandardError=append:…/<名字>.systemd.log` |
| `EnvironmentVariables` | `Environment=`（變數名要一樣；值是這台機器的，`ops-sync` 不比） |
| 同一個 job 不會疊 | 同一個 unit 還在跑時 timer 不會再起一個 |

systemd 多一件 launchd 不必講的事：**收 unit 時看的是 cgroup，不是程序群**。`dev-server` 的 kick 用 detached 拉起
vite，launchd 那邊脫離程序群就活著；systemd 預設 `KillMode=control-group` 會在 kick 一結束就把 vite 一起收掉，
所以那支 unit 要 `KillMode=process`。

`ops-sync` 比 unit 用 parse 過的「段.鍵 → 值（依出現順序）」：註解、空行、行尾 `\` 接續都不算，**除了
`Environment=` 的值以外全部都比**（鍵要在，理由同 plist）。Linux 上掃的是 `~/.config/systemd/user/com.agm.*`，
不掃 `~/Library/LaunchAgents`。

Linux 有 `com.agm.browser-gc` systemd timer，但執行 Linux 專用的 `browser_gc_linux.py`：每 30 分鐘回收本使用者
已孤兒（父程序是 pid 1 或自己的 `systemd --user`）、超過 2 分鐘且沒有 CDP 連線的 headless Chrome，清理安全標記的舊 `/tmp/am-*` profile，再跑 `pane-gc`。
如果 `ss` 不存在或 CDP 狀態無法查明，會保留程序。unit 帶 `HERDR_SESSION=agents-manager`（跟 daemon-update 一致），尾端 `pane-gc.sh` 才連得到 daemon 用的 herdr server；`herdr pane list` 回的不是 JSON 或是 error JSON 時，pane-gc 只在 `browser-gc.log` 記一行原因就收，不噴 traceback。它不啟動 browser-gc bot，也不派 ego-browser task。
`browser-gc-kick.sh`／`browser-gc-task.md`／plist 仍只標 `darwin`；ego lite／OB 圖形 worker 依 #718 暫不實作，
所以 Linux `bin/` 裡出現 macOS 的 `browser-gc-kick.sh` 仍會報成 `extra`。

安裝（**需要 AGM 核准**，同 macOS；`<名字>` 是對照表列出的八支）：

```sh
install -m 644 scripts/ops/systemd/com.agm.<名字>.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now com.agm.<名字>.timer
loginctl enable-linger "$USER"   # 沒登入也要跑（一次就好；沒開的話登出後整個 user manager 會停）
```

`daemon-update` 的 `Environment=` 有 PATH（`%h/.local/bin:%h/.bun/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`）
與 `HERDR_SESSION=agents-manager`；新主機的工具不在這些位置就改安裝那份，`ops-sync` 不會報（`Environment=` 只看在不在，不比內容；安裝端少了 `HERDR_SESSION` 也一樣不會報，要自己重裝或補 drop-in）。
`HERDR_SESSION` 只有 systemd unit 帶：Linux 主機只跑 daemon 用的 session、沒有 default server，排程又沒有 pane 環境，
不指定的話 `daemon-swap.sh` 的 `herdr pane list` 會回 `server_not_running` 而中止換版。daemon 改用別的 session 名時這裡跟著改。

排程每 5 分鐘一輪（`StartInterval 300`／`OnUnitActiveSec=300s`），每輪都往下檢查；沒有新東西時只有一次 `git fetch`，不問 GitHub。

環境變數（都可選；unit／plist 的 `Environment` 只放 `PATH`（unit 另加 `HERDR_SESSION`，見上），要有 `git`、`gh`、`bun`、`python3`、`nice`）：

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state 都在這） |
| `AGM_REPO` | `~/project/agents-manager` | 正式 daemon 跑的那份（`daemon-swap.sh` 換它的 `target/release`）；也是專用 checkout 的 origin URL 來源 |
| `AGM_DEPLOY_CHECKOUT` | `~/.cache/agents-manager/deploy-checkout` | 專用、乾淨的 checkout（第一次自動 `git clone`）。只有這支腳本動它，不碰主樹與別人的 worktree；`target/` 與 `node_modules` 留著讓建置增量 |
| `AGM_GH_REPO` | `Eden-Sun/agents-manager` | 問 commit status 的 repo |
| `AGM_CI_CONTEXT` | `ubuntu-ci` | 看哪一條 status |
| `AGM_CI_LOOKBACK` | `30` | 沿 first-parent 往回最多看幾顆（`ubuntu-ci` 只跑最新 HEAD、會跳過中間的 sha） |
| `AM_AGENT_NAME` | `daemon-update-kick` | 租約 owner／`ops_alert` 的 source |
| `AGM_FAIL_ALERT_AFTER` | `6` | 連續幾輪「沒能完成」推 `ops_alert check_failing`（每輪 5 分鐘＝約 30 分鐘） |
| `AGM_LOCK_STALE_SECS` | `120` | 鎖沒有可查的執行者時，超過這麼久就當殘留回收；OS advisory lock 仍被持有時不回收 |
| `AGM_LOCK_HUNG_SECS` | `7200` | 執行者還活著但卡了這麼久：推 `ops_alert runner_hung`（不搶鎖；冷建置要十幾分鐘，所以給寬） |
| `AM_MAINTENANCE_ESCALATE_MINS`（daemon 端） | `30` | 只對 bot 申請的核准有意義（SPEC §18.10）；自動部署自開的單等待為 0，不會升級 |

狀態檔（都在 `AGM_DIR`）：`daemon-update.built`（上次換上去的 short sha，`daemon-swap.sh` 寫）、`daemon-update.rejected`（換上去被回滾的完整 sha，之後不再挑）、
`daemon-update.fails`（連續失敗輪數）、`daemon-update.lock`（owner 與診斷資訊）、`daemon-update.lock.guard`（核心自動釋放的防重疊 advisory lock）、`daemon-update.now.json`（立即部署請求）、`daemon-update.log`。
`.built` 不在或指向不在 repo 的 sha 時，腳本推 `ops_alert built_unknown` 並停住——它需要知道線上是哪一版才敢往上換。

同一顆 sha 建好後（`<checkout>/target/release/.built-for`）等安全窗口的那幾輪不會重建；`daemon-swap.sh` 結束碼 4（有人在忙）不算失敗，下一輪再試。
其他結束碼：10（換版**之前**中止：新 binary 的 `--version` 內嵌的 sha 不是要換的那顆、是髒樹建的、或舊 binary 沒內嵌 sha；什麼都沒動）推 `swap_binary_sha_mismatch`，不記進 `.rejected`；6（升過 schema、往前修）補寫 `.built` 並推 `swap_forward_fixed`；7（已回滾）推 `swap_rolled_back` 並記進 `.rejected`；8（換好但窗口沒交還）推 `swap_lease_not_released`；
9（線上 daemon 太舊，沒有 restart-window 路由）推 `swap_daemon_too_old`。

**首次上線／舊 daemon**：自動 kick 不帶核准 id；線上 daemon 還沒有 `POST /api/services/daemon-swap/restart-window` 時，照樣以 9 中止並回報。若執行的是已核准的舊部署工作，可在同一份 checkout 呼叫 `daemon-swap.sh --approval <id>`：只有偵測到舊 daemon（`service_old`／`bootstrap`）才會用 User token 和舊式核准租約 bootstrap；`service_old` 還必須有既存的 service token，缺檔會 fail closed。新版路由存在時接受但忽略這個相容參數，改走 service principal。首換成功後新 daemon 建立 service token，之後自動換版使用新路徑。腳本會先確認同一 checkout 的 `daemon-start.py` 可讀，否則在停 daemon 前以 3 中止。

### 立即部署（使用者 2026-09-25，SPEC §18.2）

網頁左上角按「立即部署」時，daemon（`POST /api/deploy/now`）驗過目標後寫 `daemon-update.now.json`（`{sha,live_sha,requested_at,requested_by}`；**不開核准單**），再 `launchctl kickstart gui/<uid>/com.agm.daemon-update`
（Linux：`systemctl --user start --no-block com.agm.daemon-update.service`——`--no-block` 是因為 oneshot 的 start 會等 kick 整輪跑完；
daemon 沒有 `XDG_RUNTIME_DIR` 時補 `/run/user/<uid>`，issue #677）。kick 讀到這個檔：

- **部署那顆 sha，不等 `ubuntu-ci`**（使用者按下就是明確要求）；其餘（建置、`daemon-swap.sh` 的安全條件）與例行相同。
- sha 要是 origin/main 的祖先（否則 `ops_alert now_target_invalid`）、線上版本要是它的祖先（否則 `now_target_older`，拒絕降版）；`.built` 到它之間沒有程式碼差異就收掉請求（已經是最新）。
- 做完（含往前修、回滾）才刪請求檔；`daemon-swap.sh` 回 4（有人在忙）就留著，下一輪再試。請求檔壞掉 → `ops_alert now_request_corrupt`、刪掉。

daemon 的 `kick_ready` 以「裝好的 kick 裡有沒有 `daemon-update.now.json` 這個字」判斷，所以**要先 install 這一版 kick，按鈕才按得下去**。

### 隔離測試

不要對正式 daemon 測。開一個獨立 daemon 與獨立資料目錄：

```sh
AM_DATA_DIR=/tmp/am-ops-test ./target/release/agents-managerd serve --port 7799 &
mkdir -p /tmp/am-ops-test/supervisor/AGM/bin
# 把 bin/agm 指到測試 daemon（runtime.json 的 daemon_url 寫 127.0.0.1:7799）
AGM_DIR=/tmp/am-ops-test/supervisor/AGM AGM_REPO=$PWD AGM_DEPLOY_CHECKOUT=/tmp/am-ops-test/checkout \
  AGM_SWAP_SCRIPT=/bin/true bash scripts/ops/daemon-update-kick.sh
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
```

`daemon-swap.sh` 與 `daemon-start.py` 不裝：kick 從專用 checkout（要換上的那顆 sha）跑它們。

## daemon-swap.sh（＋ daemon-start.py）

自動部署換 binary 用的那一段：拿 restart 窗口 → 備份 DB → 換 binary → 重啟 → 驗證 → 寫 `.built`。
以前每趟由建置 child 在 scratchpad 臨時寫一份，2026-09-20 就因為把 `user_version` 寫死成 10（那批升到 11）
誤判成失敗、回滾、舊 binary 被版本閘擋下，daemon 停了 33 秒。所以它進了版控，行為由
`daemon-swap_test.sh` 釘住：

- 預期 schema 版本從 checkout 的 `SCHEMA_HISTORY` 讀，不寫死；讀不到就中止。
- 回滾還原 DB 前先停 daemon、清掉 `-wal`／`-shm`，還原後自驗 `user_version` 與 `integrity_check`。
- 所有部署驗證成功後，保留這趟 DB 備份並清除同一 DB 的舊 `.bak-*`；abort／rollback 不清理備份。
- 停 daemon 一律走 `stop_daemon`：TERM、最多等 30 秒、還在就 KILL。rollback（含往前修那次重啟）以前只 TERM 後固定 sleep 5，新 daemon 還沒退就覆蓋 binary、刪 `-wal`／`-shm`、蓋掉 DB；現在跟換版主線同一套。
- 升過 schema 的失敗**預設往前修**（沿用新 binary，exit 6），只有新 binary 起不來才還原 binary＋DB（exit 7）。
- 啟動走 `launchctl submit` ＋ `daemon-start.py`（fork + setsid）：daemon 是 ppid=1、nice 0。
  在 pane 裡直接背景起會繼承 pane 忙碌時的 nice 5，非 root 降不回去。
  Linux（issue #677）走 `systemd-run --user --collect --unit=am-daemon-swap-<pid>-<第幾次> -p Type=forking -p KillMode=process`
  跑同一支 `daemon-start.py`：transient unit 對應 `launchctl submit`，不必另外裝 unit 檔。`Type=forking` 讓 systemd 把 fork 出來的
  daemon 認成 main PID；`KillMode=process` 是因為 systemd 收 unit 時看 cgroup，setsid 脫離不了，預設會連 daemon 起的子行程一起殺。
  平台看 `uname -s`（測試用 `AGM_OPS_PLATFORM` 蓋掉），沒有 `XDG_RUNTIME_DIR` 時補 `/run/user/<uid>`。
- 3b 自測 prompt 送給固定的自測對象（`SWAP_PROBE_BOT`，預設 AGM 的 browser-gc child）；對方**沒有在跑**（`no active run`，例如 Linux 主機沒有桌面所以它 offline）就略過並在 log 寫明，不卡住自動部署；其他非 200 仍中止。
- 換 binary 前必須讀到非空的 `agm state` bot 名單；命令失敗或名單空白會 exit 3 並交還 restart lease。
- 重啟後比對 bot 名單（看 id）：少了就回滾，**只有**換版窗口內刻意刪掉的不算——deleted_at 在窗口起點之後、
  而且有刪除 API 留下的 `delete_bot`／`delete_project` intent（subject 是它、它的專案，或 payload 快照裡有它），DB 唯讀查。
  只有 deleted_at、沒有 intent（重啟後 reconcile 退役、投影軟刪）照樣回滾（issue #553：2026-09-24 父 bot 在窗口內刪 child i263 被誤判回滾）。
  父 bot 用 `herdr pane close` 收 child（不呼叫 DELETE）時，daemon 退役那顆會寫 `retire_child` 紀錄；只認 subject 是它、窗口內、
  `cause` 是 `pane_closed`（herdr 報過關閉事件、當下 pane 也不在）或 `promoted` 的。開機 reconcile 的 `unconfirmed` child 另有窄例外：
  intent 說 pane 已 gone、父 bot 仍有 active run 且留在 after 名單，腳本最多重讀 5 秒等 intent 落地；母 bot 消失、pane 還在的 `agent_missing`、
  `herdr_restarted` 與其他 `unconfirmed` 仍回滾（issue #834，判準見 SPEC §6.5a）。
- `agm supervisor` 讀取失敗、status 空白或不屬於 `starting`／`idle`／`busy` 都回滾；新版 bot 名單也必須成功讀回且非空。
  例外（issue #771）：停 daemon 前先讀一次 status；**換版前就是 `waiting_quota`、換版後仍是**視為額度等待、與新版無關，不回滾（log 會寫明）。換版前健康、換版後才 `waiting_quota`，或換版前讀不到，照樣回滾。
  以前因這條被回滾而進了 `daemon-update.rejected` 的 sha（2026-10-02 的 09a2438d）不會自動解除：確認它是無辜的後，從該檔刪掉那一行（`grep -vx <完整 sha> daemon-update.rejected`）下一輪就會再挑。

```sh
scripts/ops/daemon-swap.sh --sha <完整 sha> --old <short sha> --old-hash <sha256 前 16 碼> \
    --owner <窗口持有者名稱> --checkout <乾淨 checkout> [--approval <舊流程核准 id>]
```

**不需要核准單**（使用者 2026-09-29）：窗口由 daemon 的 `POST /api/services/daemon-swap/restart-window` 開（daemon-swap 服務身分自己開一筆立即核准的 restart 單，再走同一個 acquire——
沒人 working／送達中、沒有別人的租約才拿得到，拿到時暫停 assignment 派送）。沒有自己的 pane（排程跑）時，3b 改用 `herdr pane list` 確認 socket 通、協定對得上。

離開碼：0 成功、2 參數錯、3 前置核對失敗（含啟動器缺失，保證舊 daemon 尚未停止）、4 沒窗口／複查不安全、5 備份有問題、6 往前修後停在新 binary、7 已回滾、8 換版成功但 restart 窗口沒交還成功（issue #477）、9 daemon 太舊且沒有明確 `--approval`（自動 kick 不傳核准 id）。**窗口不會自己消失**：它要撐到租約的 `expires_at`——預設 900 秒（`maintenance::DEFAULT_TTL_SECS`，上限 3600，且不會晚於那張核准的到期時間；自開的單有效期＝ttl＋5 分鐘），這段時間內 supervisor 的 assignment 派送是停的、也沒有人拿得到 restart 窗口。看到 8 就是要有人處理：等 TTL 到期，或請 AGM 用 `lease release restart --force` 附理由接管，不要當成換版順利結束。

`lease_token` **不進 argv**（issue #477）：拿到窗口之後 `mktemp` 在 AGM 私有目錄底下建一個 0600 的檔（不可預測路徑、不落在全域可寫的 /tmp），`agm lease release` 走 `--lease-token-file` 讀，腳本結束時（不管成敗）刪掉。argv 對同一個 uid 的行程是公開的（`ps`），而那顆 token 是「只出現一次、任何 API 都查不到」的一次性憑證，抄走就能收掉別人正在換 binary 的窗口。交還的 rc 也不再被 `>/dev/null 2>&1` 吞掉——以前失敗時 log 照樣寫「窗口已交還」，而窗口其實握到 TTL。

## claude-release-kick.sh

舊的排程入口保留為相容 wrapper，轉呼叫 `release-triage-kick.sh`。Claude binary diff 已併入 changelog 分診同一則交辦及通知；
獨立腳本不再另派工。狀態檔仍是 `claude-release.last`，由統一 kick 在 binary diff 派成功後更新。隔離測試：
`bash scripts/ops/claude-release-kick_test.sh` 驗 wrapper；合併內容和狀態由 `release-triage-kick_test.sh` 驗。

既有 launchd／systemd 的 `com.agm.claude-release` 排程可繼續呼叫 wrapper；它與 `com.agm.release-triage` 同時觸發也共用 `release-triage.lock`，不會重疊派工。

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
`pending` 是空的且沒有新 Claude binary 時就安靜結束、不寫 log。若 `CLAUDE_VERSIONS_DIR` 出現新版本，kick 會把 binary diff 補充任務及新舊路徑附到同一份正文；即使 changelog 沒有待分診列，也會派一則 binary-only 交辦。任務內容在 `release-triage-task.md`，Claude binary diff 指示在 `claude-release-diff-task.md`；`claude-release-task.md` 保留給更新框的手動解析入口。

| 變數 | 預設 | 意義 |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | 總管 cwd（`bin/agm`、log、state 都在這；task 檔也放這） |
| `AGM_REPO` | `~/project/agents-manager` | 找 `target/release/agents-managerd` 的 repo（可被 `AM_BINARY` 整個蓋過） |
| `AM_BINARY` | `$AGM_REPO/target/release/agents-managerd` | 呼叫 `release-triage-check` 的二進位路徑 |
| `AGM_RELEASE_BOT` | `runtime.json` 的 `release_bot_id`，沒有就 `responder_bot_id` | 派給誰；**不能是巡檢自己**。查不到就跳過，不亂派 |
| `AGM_TRIAGE_QUOTA_MAX` | `85` | 被派的那顆 bot 的 5h 用量 ≥ 這個百分比就不派 |
| `AGM_LOCK_STALE_SECS` | `120` | 鎖沒有可查的執行者時，超過這麼久就當殘留回收；OS advisory lock 仍被持有時不回收 |
| `AGM_LOCK_HUNG_SECS` | `3600` | 執行者還活著但卡了這麼久：推 `ops_alert` 喊人（不搶鎖） |
| `AGM_EXTRA_PATH` | `/opt/homebrew/bin:/usr/local/bin` | 腳本開頭補在 `PATH` 前面的目錄；只給測試蓋掉 |
| `CLAUDE_VERSIONS_DIR` | `~/.local/share/claude/versions` | Claude binary 版本目錄；沿用 `claude-release.last` 偵測舊版到新版 |

行為重點：

- **一則交辦最多 5 版／kept＋unmatched 合計 80 條**，順序照 JSON；超過的留給下一輪（JSON 多帶 `deferred_versions`）。第一版一定帶，單版超量也不會永遠卡住。
  request-id 是 `release-triage-<kind>-<to>`；被截斷時 `<to>` 用這批最後一版，下一批才不會撞同一個 id 被 daemon 去重吞掉。
- **額度閘門**：派之前用 `bin/agm state`＋`bin/agm quota` 找被派 bot 的身分那一格（`<kind>:<identity>`，沒有就 `<kind>`），
  5h ≥ `AGM_TRIAGE_QUOTA_MAX` 或有 `limit_hit` → 不派、記一行 log、列維持 `pending`，下一輪再看。查不到（端點壞、找不到那格）**照派**並記 log，
  不因為端點壞了就永遠不做。只在真的有 changelog pending 或 binary diff 時才查，一輪只查一次。
- **鎖**（補 #66 留言的洞）：`release-triage.lock` 裡寫 pid＋時間（同 `daemon-update-kick.sh` 的格式），`release-triage.lock.guard` 用 OS advisory lock 串行化新舊鎖建立與殘留回收，避免重複派工。執行者不在（含 pid 被別的程序重用）且沒有行程仍持 advisory lock 時就回收接手；
  活 runner 在 `AGM_LOCK_QUIET_SECS` 內安靜跳過，超過 `AGM_LOCK_HUNG_SECS` 推 `ops_alert`（`runner_hung`）；無法驗證的 guard owner 長時間仍持鎖也會告警；回收不掉推 `stale_lock`。
- **依賴**（補 #66 留言的洞）：開頭自補 `PATH=/opt/homebrew/bin:/usr/local/bin:$PATH`；找不到 `python3` 推 `ops_alert`（`missing_dependency`）並寫 log，不靜默 `exit 0`。
  只依賴 `python3` 與 `bin/agm`（額度走 `agm quota`，不用 `curl`／`gh`；開 issue 是 daemon 的事）。
- **派成功要寫回帳本**：`assign` 成功後對**這一則實際帶出去的版本**（截斷後那批，不是全部 pending）呼叫
  `bin/agm release-triage dispatched --kind <k> --version <v>…`，帳本才會離開 `pending`（下一輪不再回同一批、「`dispatched` 超過 6 小時沒 verdict 退回 pending」的計時才會開始）。
  `assign` 失敗不標；`dispatched` 本身失敗只記一行 log、照舊 `exit 0`，**不重派**（request-id 會擋住重複交辦）。
- **`publish` 重試**：每輪（不論有沒有 pending、額度擋不擋）對兩個 kind 各呼叫一次 `bin/agm release-triage publish --kind <k>`，
  給 gh 失敗停在 `judged` 的版本一個重試入口（daemon 端不做定時器）。沒東西要重試（results 空、`disabled`、`deferred`）時安靜；只有 `published`／`failed` 才寫 log。
  舊的 `bin/agm` 不認得 `release-triage`（argparse exit 2）：log 講一次就略過，用 `release-triage-publish-unsupported` 這個 state 檔記「已經講過」，換新 agm 後自動清掉。
- 某個 kind 的 `release-triage-check` 失敗（抓不到 feed 等）只跳過該 kind 的 changelog 派工，不當成「沒有新版」，另一個 kind 照跑；若 Claude binary 同時換版，binary-only 交辦仍會送出。
- Claude binary diff 與 changelog pending 同在時合成一則 assignment；只在派工成功後推進 `claude-release.last`，並使用 `release-triage` 的公告 id。Binary-only 用 `agm-claude-release-<版本>-notice`，避免已先送出的 changelog 通知與不同正文共用 ID。

**跟其他 kick 的分工**：

- `claude-release-kick.sh`：既有 launchd 的相容入口，單純呼叫本腳本；binary diff 由本腳本合併進 changelog 分診，不再各派各的。
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

重疊：`com.agm.release-triage` 與相容入口 `com.agm.claude-release`（`claude-release-kick.sh` `exec` 進同一支腳本）兩個 timer 會在同一秒到點，後到的撞上鎖就安靜跳過；
只有鎖已存在超過 `AGM_LOCK_QUIET_SECS`（預設 60）才記「已有執行者」，超過 `AGM_LOCK_HUNG_SECS` 仍照舊推 `runner_hung`。

隔離測試：`bash scripts/ops/release-triage-kick_test.sh`（假的 `bin/agm`／`agents-managerd`，含 `env -i PATH=/usr/bin:/bin` 模擬 launchd、殘留鎖與活鎖、額度閘門）；
`release-triage-check` 本身的切條與分桶由 daemon 的 `cargo test` 釘住，不在這裡重測。

正式安裝（**需要 AGM 核准；而且要等 daemon 端的 `release-triage-check` 上線**，沒有那個子命令這支每輪都只會記「檢查失敗」）：

```sh
install -m 755 scripts/ops/release-triage-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/release-triage-task.md ~/.config/agents-manager/supervisor/AGM/
install -m 644 scripts/ops/claude-release-diff-task.md ~/.config/agents-manager/supervisor/AGM/
install -m 644 scripts/ops/claude-release-task.md ~/.config/agents-manager/supervisor/AGM/
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

main 的 CI 盯哨（issue #211）。issue #716 起預設看 commit status `ubuntu-ci`（`AGM_CI_SOURCE=ubuntu-ci`，失敗 log 本機 `AGM_CI_LOG_DIR` 有就讀、否則 ssh `AGM_CI_HOST` 取），下面的 `gh run …` 是 `AGM_CI_SOURCE=actions` 的舊行為。2026-09-16 起 main 的 CI 連紅好幾天沒人發現——規則只要求跑本機 `check.sh`，沒人看 GitHub 的結果。
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

- `outbox-gc.sh`：launchd `com.agm.outbox-gc` 每 10 分鐘；刪 outbox 底下（含 bot 子目錄）mtime 與 ctime 都超過 60 分鐘的檔（`mv`／`cp -p` 進來的舊檔從搬入起算）與空目錄；有 `.am-share-keep` 標記檔的 bot 目錄（分享用 bot，SPEC §20.5；daemon 放的）整個跳過；`OUTBOX_GC_NOW` 是測試用的時鐘接縫。護欄：`AM_OUTBOX_ROOT` 不在 `~/.config/agents-manager/outbox*` 就拒絕。
- `pane-gc.sh`：關卡住超過 24 小時的 `claude auth login`／`gcloud auth login`／`codex login` pane；幽靈 pane 只記錄。由 `browser-gc-kick.sh` 呼叫。
- `browser-gc-kick.sh`：launchd `com.agm.browser-gc`，**`StartInterval 1800`（30 分鐘）**；收孤兒、無 CDP 連線、活超過 2 分鐘的 headless Chrome，刪沒人用的 `/tmp/am-*` Chrome profile，跑 `pane-gc.sh`，再派 `browser-gc-task.md`。有鎖與殘留回收（issue #490）。
  （這裡原本寫「每 6 小時」，但實機一直是 1800 秒——是**文件寫錯**，不是排程跑錯；issue #487 把 plist 收進版控時照實機現值定案。）

隔離測試：`bash scripts/ops/outbox-gc_test.sh`、`pane-gc_test.sh`、`browser-gc-kick_test.sh`（假 `ps`／`lsof`／`herdr`／`bin/agm`，`kill` 用函式替身，`/tmp/am-*` 換成暫存目錄；不會殺行程、關 pane 或碰真的 outbox／HOME）。
安裝比照其他 kick：`install -m 755 scripts/ops/{outbox-gc,pane-gc,browser-gc-kick}.sh ~/.config/agents-manager/supervisor/AGM/bin/`、`install -m 644 scripts/ops/browser-gc-task.md ~/.config/agents-manager/supervisor/AGM/`；launchd 由巡檢處理。

## dev-server-kick.ts（issue #418）

5173 dev server 的看門狗。**2026-09-24 之前只存在於 `AGM/bin/`**：`docs/SPEC.md` §18.1 把行為寫得很細，
程式碼卻沒有版控、沒有 review、沒有測試，而且它會 `kill` 占用 port 的行程。現在 repo 是來源檔。

- launchd `com.agm.dev-server`（Linux：systemd user timer 同名）：`StartInterval 60`、`RunAtLoad`，跑 `bun run …/AGM/bin/dev-server-kick.ts`。
- 5173 的 worktree＝`${AGM_REPO:-~/project/agents-manager}-main`（`AGM_DEV_REPO` 可整個指定）；誰在聽 port：macOS `lsof`、Linux `ss`（#676）。
- 行為與判斷順序見 `docs/SPEC.md` §18.1（健康＝綁 `*`／`0.0.0.0` 且本機 curl 有回應；只收孤兒 vite；
  找不到 node 或 `vite.js` 寧可這輪不起，也不拿 bun 代跑）。
- **看門狗用 bun、vite 一律用 node**：bun 的 upgrade socket 沒有 `destroySoon`，daemon 一重啟代理斷線 vite 會 crash。

隔離測試：`bash scripts/ops/dev-server-kick_test.sh`（假 `lsof`／`ss`／`ps`／`git`／`bun`／`node`，副本的 `PORT` 換成
測試 port，假 `lsof`／`ss` 只回報測試自己 spawn 的 pid，整套情境 lsof 與 ss 各跑一次；**不會碰真的 5173 或真的 vite**）。沒有 bun／python3 會自己 skip。

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
- 收 server 不再用 `pkill -f 'herdr.*server'`：那個 pattern 會連 argv 裡剛好提到 herdr／server 的 claude／codex bot（長 persona）、
  ssh、`herdr pane list server` 一起殺。現在用 `ps -axo pid=,args=` 認：第一個字 basename 是 `herdr`、跳過旗標
  （`--session`／`--machine` 連值）後第一個位置參數是 `server`，只對這些 pid 送 TERM、5 秒後還在的補 KILL（macOS／Linux 兩段共用）。
- 路徑與 uid（`gui/501`）寫死成這台開發機的值，是收進 repo 時保留的既有行為。
- **Linux 主機**（issue #677）走檔案開頭那段：herdr server 由 systemd user unit `herdr@<session>.service`
  （`scripts/ops/systemd/herdr@.service`，跟 `com.agm.*` 一起由 ops-sync 對照安裝）看管。記下原本 active 的
  `herdr@*.service` → 先 stop 再殺殘留 server → 清 `$HOME/.config/herdr` 的 socket → start 回同一批、`is-active`
  沒回來就 rc=1；default 原本在跑才補起。路徑一律從 `$HOME` 推，沒有 `XDG_RUNTIME_DIR` 時補 `/run/user/<uid>`。
- `herdr-upgrade.sh` 只適用 macOS（brew 換 binary、靠 keg 回滾），在 Linux 一開始就 exit 2、什麼都不動。

隔離測試：`bash scripts/ops/herdr-full-restart_test.sh`（`launchctl`／`systemctl`／`kill`／`ps`／`sleep`／`herdr`／`uname`
全部用**注入的 shell 函式**攔截，不靠 PATH；socket 與 plist 都在暫存目錄；macOS 與 Linux 兩段各有一組情境）。

安裝（**需要 AGM 核准**）：

```sh
install -m 755 scripts/ops/herdr-full-restart.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

## project-transfer（issue #710）

單一專案從一顆 daemon 移交到另一顆（例：Mac → agm-host，以遠端主機 `m4p` 接手）。不安裝，從 checkout 跑；
流程、改寫規則與拒絕條件見 SPEC §11.9。`export` 只讀來源 DB（先做 backup 快照、讀完刪）；`import` 要**目標 daemon 停著**
（拿同一把 `daemon.lock`），先 `--dry-run` 看摘要。專案改在目標本機跑：`--host local --path-map /來源=/目標`，原生對話先用下面的 transcript-transfer 搬（SPEC §11.9a）。
協調者（AGM）的資料：export 與 import 都加 `--with-supervisor`（合併規則見 SPEC §11.9b）；群組任務一律跟著專案走。
隔離測試：`scripts/ops/project-transfer_test.sh`（假 DB，schema 從 `daemon/src/db.rs` 抽）。

## transcript-transfer（issue #717）

專案改在接手那台本機跑時，把 bot 的原生對話檔（claude／codex／grok）搬到那台並改寫路徑，讓 `start?resume=native` 接得回。
在來源機器跑、接在 `project-transfer export` 之後，輸出改好 `runs.transcript_path` 的新 bundle 給 `import --host local --path-map …`。
不安裝，從 checkout 跑；實測結果與 runbook 見 SPEC §11.9a。先 `--dry-run`；結束碼 2＝有找不到／拒絕／衝突。
隔離測試：`scripts/ops/transcript-transfer_test.sh`（假的來源 $HOME 與目標目錄，不 ssh）。

## cutover-to-host.sh（issue #721）

`Agents Manager` 與 `AGM-DM-GRUP` 整套從 Mac 切到 agm-host 本機跑（#675）。在 Mac 從 checkout 跑，不安裝；步驟、閘門、回滾與演練結果見 SPEC §11.9c。
`cutover` 預設 dry-run（唯讀檢查＋列出會做的事），`--execute` 才做、並自己脫離成背景（log 在 `~/.config/agents-manager/cutover/<時間>/run.log`）；
`rollback --state-dir <同一個>` 還原；`drill` 在目標另開目錄用 DB 複本演練、做完當下刪。輔助 `cutover-helper.py`（兩邊都跑）。
`host-state-transfer.py` 隨 cutover 搬完整非 project config、UI token、outbox 與 identity 設定目錄清單；清單只記路徑／存在狀態，不搬憑證。目標安裝與 rollback 都拿 `daemon.lock`，不動 SQLite。
隔離測試：`scripts/ops/cutover-to-host_test.sh`（兩顆假 daemon、假 ssh／rsync／launchctl、不碰正式環境，並執行 host-state 的暫存目錄測試）。

## 已安裝版與 repo 的落差（issue #418 稽核，2026-09-24）

這些檔沒有自動同步，所以會漂。2026-09-24 的逐支比對（`git hash-object` 對 `origin/main` 的 blob）結論：
同名檔沒有任何一支「安裝版比 repo 新」；`browser-gc-kick.sh` 與 `pane-gc.sh` 落後一個純寫法的 commit。

- **`herdr-lan-check.sh` 目前沒有裝**，但上面 herdr-update-kick 那節寫著要 `install`。
  實際用法（`herdr-upgrade-runbook.md`、`herdr-update-kick.sh` 的派工正文）都是從 repo checkout 跑
  `bash scripts/ops/herdr-lan-check.sh /opt/homebrew/bin/herdr`，所以沒裝不影響升級流程；
  要嘛補裝、要嘛把那行 install 拿掉，兩邊挑一個對齊。
- `daemon-swap.sh`、`daemon-start.py`、`herdr-upgrade-runbook.md` 本來就不裝（從 checkout 跑），不是缺口。

## ubuntu 背景完整 CI（issue #716）

main 的每個 push 不再等 GitHub Actions：agm-host 上的 systemd user timer 每分鐘叫一次 `scripts/ops/ubuntu-ci.sh`，
只驗最新的 main HEAD（同時一輪、中間的 sha 不補跑），跑整樹 `scripts/check.sh`，結果寫成 commit status `ubuntu-ci`、
`~/.cache/agents-manager/ci/status.json` 與 `logs/<sha>.log`。pending 送出後腳本自己中斷（checkout 失敗之類）會補 `error`
status、status.json 寫 `error`，不讓 pending 掛著（last-sha 不動，下一輪重試）；每段 `timeout -k`（`AGM_CI_KILL_AFTER`，預設 60s），
step 不理 TERM 也收得掉、不會卡住鎖。隔離測試：`bash scripts/ops/ubuntu-ci_test.sh`（本地 bare repo＋假 gh）。安裝（agm-host，需 `loginctl enable-linger ubuntu`）：

agent 本機收尾用 `scripts/check.sh changed`：Rust-only 改動在沒有 `web/dist/index.html` 的 clean checkout 會暫時建立 rust-embed stub，跑完即清除，不會因此安裝依賴或 build web。Web 檢查與背景完整 CI 仍 build 真正的 production bundle。

```sh
git clone git@github.com:Eden-Sun/agents-manager.git ~/.cache/agents-manager/ci/repo
mkdir -p ~/.config/systemd/user
cp ~/.cache/agents-manager/ci/repo/scripts/ops/ci/ubuntu-ci.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now ubuntu-ci.timer
```

runner 用的是常駐 clone 裡**當下 checkout 的** `ubuntu-ci.sh`，所以腳本本身的修改會在下一輪自動生效。
