# `check.sh changed` 耗時與 ops 選測

2026-11-23，Ubuntu，起點 `aec4ef485`。在隔離 worktree 使用受管 Cargo shim，先暖編，再對 `crates/am-lifecycle/src/lifecycle/screen.rs` 與 `daemon/src/api.rs` 各做兩次單行註解改動。每次先走負載與記憶體閘，再計 `cargo test -p agents-managerd --no-run --timings` 的牆鐘。所有改行都已還原。Rust 與 ops 基準有部分時間重疊，且編譯池與其他工作共用。

| 位置 | 牆鐘秒 | Cargo 報告秒（約） | daemon lib test 編譯秒 | 可比性 |
| --- | ---: | ---: | ---: | --- |
| 暖編 | 93.30 | 87 | 75.33 | 準備樣本 |
| lifecycle 1 | 93.33 | 87 | 76.96 | 熱池全樹 touch，其他 crate 也重編 |
| lifecycle 2 | 92.51 | 86 | 80.29 | 只 touch 一檔，重編 lifecycle 與 daemon |
| daemon 1 | 91.80 | 86 | 79.57 | 切換檔案時也同步還原 lifecycle，連帶重編 |
| daemon 2 | 70.88 | 66 | 66.09 | 只 touch 一檔，只重編 daemon |

有效單檔樣本各只有一筆。牆鐘包含 shim，Cargo 報時之外約 5–7 秒，涵蓋啟動、來源同步與結束，這幾筆沒有排隊訊息。負載閘在計時外。Cargo HTML 的 lib test unit 未提供分階段資料，link 沒有單獨數字。可讀到的 production lib 區段中，lifecycle 2 的 lifecycle 前端 4.06 秒、codegen 區段 1.85 秒，daemon 前端 16.73 秒、codegen 區段 5.57 秒；daemon 2 的 daemon 前端 15.75 秒、codegen 區段 5.15 秒。這些區段和 lib test unit 有重疊，不能加總或拿來推算 lib test 的 link 時間。

daemon lib test 位於兩種編譯的關鍵路徑。起點的 `daemon/src/lib.rs` 在測試模式仍以 21 處路徑匯入 am-lifecycle 來源；`runners/am_base_tests` 也掛載 am-base 測試。測試過濾只減少執行量。現有 dev/test 設定已縮減除錯資訊（workspace line tables、依賴無 debuginfo），目前沒有證據支持改 linker、codegen-units 或 incremental。優先完成 #1862 的 lifecycle 撤掛載後再以專用名額重測，這一輪不改 L2 檔案。若要再比較 profile，需在同一熱池成員固定輸入與參數，每組預熱後重複取樣，另取得 lib test link 的直接量測。

## Ops 測量

同一台 Ubuntu、固定 `CHECK_OPS_JOBS=4`，使用 `scripts/check.sh ops <before> <after>` 回放歷史路徑。每種選測各量兩輪，含共用 lint、canary 與選中的 ops 測試。此表只量 ops，不含 daemon 或 web。

| 路徑 | 測試數（前 → 後） | 前兩輪（秒） | 後兩輪（秒） | 中位（前 → 後，秒） |
| --- | ---: | ---: | ---: | ---: |
| Ledger schema（`47a65d77d^..47a65d77d`） | 66 → 5 | 181.22、176.99 | 110.07、108.30 | 179.11 → 109.19（−39%） |
| Extensionless project-transfer（`64ea6cd29^..64ea6cd29`） | 66 → 4 | 181.22、176.99 | 22.22、20.71 | 179.11 → 21.47（−88%） |

舊選擇器對兩組路徑都回 `__all__`，因此兩列共用同一組全套基準。總測試數是 Bash 或 Python wrapper 數，測試程序內的測試案例可能更多。基準與部分 Rust 測量重疊，無獨占負載。差值是這次觀測，不是端到端速度保證。

schema 路徑仍選 daemon。ops 保留讀真實 schema 的 `daemon-swap_test.sh` 和 `project-transfer_test.sh`，再加三支共用守門測試。無副檔名的 project-transfer 保留自己的測試與三支守門測試。若指定測試缺失、路徑不認得、或修改分派器／護欄，維持全套 ops。原始日誌位於 Ubuntu 的 `/tmp/kx-check-speed.qBa461/`。

## #1862 lifecycle 撤掛載第一批

基底 `9e36fc7a5`。從 daemon 測試的 `#[path]` 改用 `am_lifecycle` 的 `child_done` 與 `daemon_notice`。既有 daemon 整合測試留原處。其餘 19 處掛載仍在。

同機、同一個 `lifecycle/screen.rs` 改一行，依舊走 Cargo shim 與負載／記憶體閘。牆鐘含 shim 同步與名額等待；lib test 秒數取自 Cargo timings。撤前與撤後都先暖編，暖編不列入比較。

| 樣本 | 熱池 | 牆鐘秒 | daemon lib test 秒 | 判讀 |
| --- | --- | ---: | ---: | --- |
| 撤前 1 | shared | 82.37 | 71.32 | 只同步一檔 |
| 撤前 2 | shared | 84.17 | 73.69 | 只同步一檔 |
| 撤後 1 | shared-2 | 885.95 | 642.66 | 排隊 159 秒、換池並同步兩檔，排除 |
| 撤後 2 | shared-2 | 110.39 | 92.07 | 只同步一檔，但與撤前不同池 |

撤前 lib test 中位數 72.51 秒。撤後單檔只有一筆，且共用熱池成員不同，無法判定編譯速度變化。兩處撤掛載仍會因上游 am-lifecycle 更新而重編 daemon lib test。撤後暖編耗時 782.40 秒，含改 crate 根模組後的重建，不視為單檔增量樣本。

daemon 的 lib、bin、`api_doc_parity` 執行檔各自比對 `--list` 與 `--list --ignored`，前後逐位元組相同。lib 6,208 條（4 ignored）、bin 0 條、parity 1 條。其他三個套件的來源與 feature 均未改，本批未另外重跑其清單。掛載檢查通過，`api_doc_parity` 1/1。日誌與清單在 Ubuntu 的 `/tmp/kx1862-unmount.T7dRqX/`。

`ask_answers` 與 `default_session` 即使測試已分家，仍透過 `events::ports` 使用生命周期介面。該處簽名也引用 `fence`、`turn_controller` 等尚有兩份的型別。待 L2 hookrecv 與 codex_live 合流後，統一介面與測試 gate，再撤含這些依賴的掛載。大型撤掛載之後需要固定熱池成員重測，並區分減少 lib test 編譯成本和避免依賴重編。

`CHECK_MAX_LOAD=40 scripts/check.sh changed` 在量測基底 exit 0，daemon 全套 6,204 passed、4 ignored，parity 1/1。更新基底時上游合入 L2-3c 與 Grok 修正，未碰這批兩個檔案。前後量測與測試清單仍以表列基底為準。

## #1862 ask_answers 撤掛載

基底 `8b42f8e47`。daemon 在測試也使用 `am_lifecycle::ask_answers`。這個模組原先透過 `TurnCommands` 發出 `message_added`，App 的實作會轉呼叫 lifecycle 的同一個 helper。因此改用共用的 `Emit`，透過 `Emit` 發出同一個事件名稱與 payload，保留原訊息交易與事件格式。純測試及 daemon 整合測試各留原處。

同一 Ubuntu shim 熱池成員 `shared`，改 `lifecycle/screen.rs` 一行。負載／記憶體閘在牆鐘計時外；沒有獨占編譯名額。兩邊暖編各成功一次，未列入改行樣本。暖編與撤後第二輪曾收到 exit 143，保留中斷日誌並重新執行。

| 樣本 | 牆鐘秒 | daemon lib test 秒 |
| --- | ---: | ---: |
| 撤前 1 | 85.71 | 71.76 |
| 撤前 2 | 107.96 | 94.16 |
| 撤後 1 | 118.34 | 97.61 |
| 撤後 2（中斷後重試） | 103.86 | 97.56 |

lib test 仍重編，中位數由 82.96 秒到 97.59 秒。這批沒有量到加速；撤前兩輪本身相差 22.40 秒，樣本數與共用負載不足以把差值歸因於這次撤掛載。撤後第二輪用相同輸入接續先前被中斷的 build。

daemon 三個測試執行檔的 `--list` 與 `--list --ignored` 前後逐位元組相同。掛載檢查通過，`api_doc_parity` 1/1。日誌與清單在 `/tmp/kx1862-ask.aFPwAF/`。

`changed` 的 am-lifecycle 689/689（含 ask_answers 七條）、scratch guard 8/8 通過。daemon 全套在約 1,495 項通過後以 143 中斷，沒有總結，因此候選不推 main。驗證以表列基底為準，最新 main 已合 L2-3f。

續驗 `04b078be0` 時，daemon 的 ingress ports 分層守門測試拒絕直接呼叫 lifecycle helper。候選改由既有 `Emit` port 發出相同 payload，守門測試維持原樣。上表撤後時間只適用於修正前候選。

修正候選 `1af212160` 在 herdr shell pane 取得 `changed` exit 0。am-lifecycle 720/720、scratch guard 8/8、daemon 6,153 passed、4 ignored、API parity 1/1。原先失敗的 ingress ports 守門測試通過。其後只更新本文並 rebase 到純前端變更，沿用這輪 Rust 結果。

## #1862 handoff／codex_history 撤掛載

基底 `d8c99894d`。daemon 測試與正式建置都使用 `am_lifecycle` 的 `handoff` 和 `codex_history`，剩下 16 處 am-lifecycle 路徑掛載。原候選的撤後暖編遇到三個 E0117。daemon 仍掛載 `arc_ports.rs`，而 `HistorySource`、`CodexHistoryHost`、`CodexHistoryState` 改為來自 am-lifecycle，導致 daemon 嘗試為外部 trait 和外部型別 `Arc<T>` 定義 impl。三段實作原樣移到 trait 所在的 `codex_history.rs`，daemon 測試仍透過 `test-hooks` 避免啟用真實 history source。

在同一 Ubuntu shim 熱池成員 `shared` 上，兩邊各暖編一次，再分別改 `crates/am-lifecycle/src/lifecycle/screen.rs` 一行，執行 `cargo test -p agents-managerd --no-run --timings`。負載和記憶體閘在計時外，編譯名額為共用。舊基底的樣本未納入。

| 樣本 | 牆鐘秒 | daemon lib test 秒 |
| --- | ---: | ---: |
| 撤前 1 | 88.02 | 77.09 |
| 撤前 2 | 76.88 | 65.57 |
| 撤後 1 | 86.30 | 73.57 |
| 撤後 2 | 83.18 | 71.70 |

牆鐘中位數由 82.45 秒到 84.74 秒，daemon lib test 仍重編。本批未量到加速。兩次取樣和共用熱池的結果僅適用於這個中間步驟。

daemon 的 lib、bin、`api_doc_parity` 三個測試執行檔，前後的 `--list` 和 `--list --ignored` 皆逐位元組相同。lib 6,157 條，含 4 條 ignored，bin 0 條，parity 1 條。掛載守門通過。`CHECK_MAX_LOAD=40 scripts/check.sh changed` exit 0，daemon 6,153 passed、4 ignored，lifecycle 720/720、scratch guard 8/8、API parity 1/1。日誌在 `/tmp/kx1862-orphan.brDxUq/`，原 E0117 日誌在 `/tmp/kx1862-base3.HbBiFY/after-warm.log`。

## #1862 L3 中層與上層撤掛載（翻轉）

基底 `d132e8202`（含 P1a `85ccd35a9`、P1b `6bdfad9b7`、P2+P3 `a080feda9`／`7de701a60`／`5a18db271`／`33a7d465c`、P4 `7455a1483`）。這一刀撤掉 `daemon/src/lib.rs` 剩下的 16 處 am-lifecycle `#[path]` 掛載：lifecycle（含 fence、turn_controller）、hookrecv、judge、tui_prompts、codex_live、codex_model_migration、blocked_reason、background_jobs、turn_error、child_alerts、dangerous_rm、compaction、events、arc_ports、ports_impl、default_session。daemon 測試改用 `am_lifecycle`（`test-hooks`）本體，lib.rs 不再有任何 `#[path]`。

在同一 Ubuntu shim 熱池成員 `shared` 上，兩邊各暖編一次，再分別改 `crates/am-lifecycle/src/lifecycle/screen.rs` 一行，執行 `cargo test -p agents-managerd --no-run --timings`。負載和記憶體閘在計時外，編譯名額為共用。daemon lib test 秒數取自第二次樣本的 timing 報告（第一次的報告被同一 target 覆寫）。

| 樣本 | 牆鐘秒 | daemon lib test 秒 |
| --- | ---: | ---: |
| 撤前暖編 | 496.69 | |
| 撤前 1 | 78.96 | |
| 撤前 2 | 81.63 | 70.94 |
| 撤後暖編 | 452.69 | |
| 撤後 1 | 84.86 | |
| 撤後 2 | 81.31 | 70.25 |

牆鐘中位數由 80.30 秒到 83.09 秒，daemon lib test 70.94 秒到 70.25 秒，本批未量到加速。改 am-lifecycle 的檔仍使下游的 daemon lib test 整個重編：撤掛載移除的是 daemon 重複編 lifecycle 原始碼，不是依賴鏈上的重編。要看撤掛載的效果，下一步應量「只改 daemon 檔」與「只改 am-lifecycle 檔、跑 `cargo test -p am-lifecycle`」兩種情境。

check.sh 守衛 `session_production_code_reaches_other_features_only_through_ports` 原本只把 `cfg(test)`／`cfg(all(test, …))` 當測試區塊，過渡 gate 下的 `restart_child_in_pane_with` 被當成正式碼；`am_base::source_mask::test_mask` 補認 test-hooks gate。

清單對帳：am-lifecycle 測試清單前後逐位元組相同（722 條）。daemon lib 由 6,166 條到 6,160 條，少的 6 條（`lifecycle::start::add_dir_args_tests` 1 條、`specialist_section_tests` 5 條）是 crate 裡 `#[cfg(test)]` 的純測試，掛載期間在 daemon 重複執行一次，翻轉後只在 am-lifecycle 執行，仍在 crate 清單內；沒有新增項。lib 的 `--ignored`、bin、`api_doc_parity` 前後相同。`rust_test_mount_test` 通過（未掛 0、harness 0）。

## #1862 L3 撤掛載後的兩個情境

撤前 `d132e8202`（`e65c5dc72^`），撤後 `e65c5dc72`。同一 Ubuntu shim 熱池成員 `shared`，負載與記憶體閘在計時外，兩邊各先暖編 daemon 與 am-lifecycle 測試，再取兩次樣本。daemon lib test 與 am-lifecycle lib test 秒數取自各樣本的 timing 報告。

(a) 改 `daemon/src/webpush.rs` 一行，`cargo test -p agents-managerd --no-run --timings`：

| 樣本 | 牆鐘秒 | daemon lib test 秒 | daemon lib 秒 |
| --- | ---: | ---: | ---: |
| 撤前 1 | 75.74 | 70.21 | 22.57 |
| 撤前 2 | 86.34 | 80.58 | 20.44 |
| 撤後 1 | 70.07 | 64.42 | 21.99 |
| 撤後 2 | 69.39 | 63.75 | 17.36 |

(b) 改 `crates/am-lifecycle/src/lifecycle/screen.rs` 一行，`cargo test -p am-lifecycle --timings`（含編譯與 722 條執行）：

| 樣本 | 牆鐘秒 | am-lifecycle lib test 秒 |
| --- | ---: | ---: |
| 撤前 1 | 25.36 | 8.69 |
| 撤前 2 | 24.58 | 8.50 |
| 撤後 1 | 22.24 | 6.87 |
| 撤後 2 | 24.22 | 7.79 |

(a) 牆鐘中位數由 81.04 秒到 69.73 秒（約 −14%），daemon lib test 由 75.40 秒到 64.09 秒（約 −15%）：daemon 測試執行檔不再重編一份 lifecycle 原始碼。撤前第二筆偏高，兩筆樣本只能看出方向。(b) 由 24.97 秒到 23.23 秒，在雜訊範圍內；crate-only 路徑本來就不經過 daemon，撤掛載不影響它。改 am-lifecycle 後跑 daemon 測試仍要整個重編 daemon（依賴鏈），只改 lifecycle 時用 `cargo test -p am-lifecycle` 約 23 秒即可得到 crate 內的回饋。

撤後改 daemon 一行仍要約 64 秒的 daemon lib test，時間在 daemon 自己的程式碼與測試（約 6,160 條）。要再下降，需要把 daemon 的熱區或測試繼續拆出去，不是 lifecycle 掛載的問題。
