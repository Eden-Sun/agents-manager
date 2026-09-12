# 全 codebase review（origin/main 3c6bf8a，2026-09-12）

三顆 fable-xhigh 審查員在唯讀副本上逐檔讀完 daemon（83k 行 Rust）與 web（33k 行 TS），
各自的完整報告：[daemon-core.md](daemon-core.md)（23 檔）、[daemon-ops.md](daemon-ops.md)
（team／hosts／herdr／quota／supervisor）、[web.md](web.md)。這份是彙整：先修什麼。

統計：確定 **84** 條（core 10、ops 44、web 30）、可能 **~100** 條。安全面沒有 token 外洩、XSS、
shell 注入（唯一例外是 `hosts[].remote_path` 未引用）。

## 總評

架構的不變量（per-bot 鎖、Turn CAS、唯一索引、store 單一來源、WS seq／resync）設計清楚。
問題集中在三類：

1. **失敗後不可重入／中間狀態沒收尾**（daemon）：herdr 不配合或中途 `?` 回傳時，run／task／team
   停在既不是 paused 也沒有 note 的狀態，使用者按「繼續」要嘛重複交付、要嘛永遠撞同一個錯。
2. **無限併行模式是後疊上去的**：好幾條路徑仍讀 `teams` 的鏡像欄位而非 `team_issues`，最嚴重的
   一條會把 task 合進別的 issue 的整合分支。
3. **web 的送出／設定路徑會丟使用者的東西**：排隊訊息在 409 時連附件一起消失、設定面板用舊值蓋掉
   剛套用的 model／effort、側欄清理 team 不經確認。

## P0（資料遺失／錯誤合併／不可逆）

| # | 位置 | 問題 | 報告 |
|---|---|---|---|
| 1 | `team_sched.rs:1615` | 無限模式 `merge_one` 用 `teams.branch` 鏡像判斷要不要切分支，#42 的 task 會合進 #43 的整合分支；#42 交付空分支 | ops S1 |
| 2 | `team_sched.rs:2565-2585, 2676-2745` | `finish` 不可重入：交付後 `start_issue(next)` 失敗暫停，resume 再交付一次；`branch` 模式把佇列丟掉直接 `done` | ops #4 |
| 3 | `team_sched.rs:2400-2406` | `commit_all` 失敗只記 note 就送審合併，未提交改動之後被 `worktree remove --force` 丟掉 | ops #23 |
| 4 | `TeamNodes.tsx:208-222` | 側欄 team「清理」✕ 不經確認，直接軟刪成員、移除 worktree | web 1 |
| 5 | `store.ts:2571-2581`、`ChatPanel.tsx:784-813` | 排隊／「中止並取代」在 409／502 時先清佇列再送，文字與附件永久遺失 | web 1 |
| 6 | `api.rs:1330-1377` | `DELETE /api/bots/{id}` 對 team 成員回 200 但沒刪，還砍掉它的 hook 目錄 | core #3 |
| 7 | `lifecycle.rs:2251, 1924`、`bulk_restart.rs:120` | 停止／重啟 default-session 匯入的 bot 會關掉使用者自己的 pane、在使用者 session 開 workspace（違反 SPEC §6.5.1） | core #4 |

## P1（主路徑正確性）

- **子 agent 原地重啟失敗 run 永留 `stopping`**，之後 prompt 409、reconcile 不救（`lifecycle.rs:2336-2364`）。core #1
- **reconcile 子 agent 名字撞到 → `?` 讓整台主機對帳中止且每 2 秒重複**（`reconcile.rs:505-563`）。core #2
- **hook 忽略 CAS 結果 + poller 鎖外 `try_fallback`** → 同回合兩則 assistant（`hookrecv.rs:557`、`lifecycle.rs:3720`）。core #5
- **排隊 prompt 送出跳過 needs_login／dialog／picker 檢查**，文字被打進選單（`lifecycle.rs:220-310`）。core #6
- **`delivery=unknown` 的 relay 已標 delivered、task 已 working，abandon+resume 永不重送**（`team_sched.rs:990-1024`）。ops #2
- **開機 `replay_host` 早於 `respawn_schedulers`**，spool 重放的 TurnDone 沒人接、task 卡 working（`main.rs:241/252`）。ops #3
- **無限模式 `dispatch.to` 短名跨 issue 撞同名**，指到別的 issue 的執行者後 task 永遠排隊（`team_sched.rs:1955`）。ops #5
- **`swap_member` 沒更新 `want_worker_bot_id`／`rescue_bot_id`**（`team.rs:3280`）。ops #6
- **無限模式 PM `abort` 走 DECISION_PAUSES 自動前進**，標錯 issue failed 而非暫停整隊（`team_sched.rs:598, 2684`）。ops #7
- **`start_issue` 失敗不回滾也不可重入**，resume 永遠撞 `checkout -b` 同名（`team.rs:1682-1743`）。ops #8
- **`grow_workers` 沒寫 team docs／.gitignore**，`TEAM.md` 被 `git add -A` 提交進 task 分支（`team.rs:3017`）。ops #9
- **無 done issue 的 team reopen 後永遠停在 `starting`**，不是 paused、無 note（`team_sched.rs:1139`）。ops #10
- **設定面板／ModelPicker 用舊值蓋掉剛套用的 model／effort，並無故 dirty**（`BotSettingsPanel.tsx:312-367`、`ModelPicker.tsx:120-132`）。web 1
- **`openSettings` 不清 team／shell 視圖**，從 team 畫面按齒輪什麼都不發生（`store.ts:974`）。web 2
- **bootstrap 第一次 refresh 失敗照樣 ready**，深連結被改成 `/`（`store.ts:754`、`routeSync.ts:116`）。web 2
- **多分頁問卷時 BlockedPanel 與 BlockedModal 同時預載**，兩份導覽鍵互相插隊（`ChatPanel.tsx:1449`、`BlockedDraft.tsx:78`）。web 1

## 安全（需產品決策）

- **開發版預設對 LAN 開放且 `GET /api/session` 對任何 peer 發 token** = 同網段無認證 RCE（`main.rs:318`、`api.rs:205-263`）。文件明寫是取捨，建議 token 只對 loopback 發。core #7
- 遠端 codex 的 `hook_token` 在 argv 上，`ps` 可見（`lifecycle.rs:775`）。core #8
- `hosts[].remote_path` 未 `sh_quote` 就拼進 `export PATH=…`（`hosts.rs:234`）。ops #26

## 可靠性／效能（摘）

- 本機 `sh -c` 逾時後子程序不殺（無 `kill_on_drop`），hung git 持 `index.lock`（`team_git.rs:82`、`github.rs:100`、`gh_auth.rs:224`）。ops #12
- 遠端 `sh()` 丟掉 timeout 參數，固定 30 秒（`hosts.rs:33`）。ops #13、#29
- grok 探測無退避，每 30 秒起一個 agent 再關（`quota_grok.rs:290`）。ops #36
- `issues_cache`、`supervisor_inbox/notes`、web `urlCache`（object URL 不 revoke）、`messages`／`loadedBots` 只增不減。ops #34/#38、web 3
- `IssuesBar` 每次 `/api/state` 都重抓 submodules 與 issues（selector 回新物件）。web 3
- WS `seq` 在 ring 鎖外配號，重連可能漏一則（`state.rs:293`）。core f

## 測試基礎

- **web 的單元測試從未進 `scripts/check.sh` 與 CI**：19 個測試檔裡 2 個跑不起來（import 無副檔名）、`updateBatch.test.ts` 紅了三天（issue #63）。
- daemon 2 個 herdr_shim 測試會被 pane 環境變數 `AM_MODEL`／`AM_EFFORT` 干擾（在 bot pane 裡跑會假失敗）。
- 三份報告各列了「最值得補的 5 個測試」，合計 15 條。

## 文件不同步（摘）

SPEC §18.6 手改 config 行為、§6.4 DELETE 未提 team 成員、§6.5.1 vs 實作；SPEC-team §6.2 task 分支命名、§7.3；SPEC §14.2 quota poller 併發；FRONTEND.md `?token=` 快取行為不存在；API.md 缺 `queued_turn`／`queued`。

## 建議順序

1. P0 的 1、2、3（team 交付正確性）與 6、7（daemon 誤刪）——都是幾十行的修法，各附了測試建議。
2. P0 的 4、5 與 P1 的 web 三條（設定面板、openSettings、bootstrap）。
3. P1 的 daemon 六條 core 項。
4. 安全：`/api/session` 只對 loopback 發 token 要使用者拍板。
5. 把 web 測試接進 `scripts/check.sh`。
