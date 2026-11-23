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
