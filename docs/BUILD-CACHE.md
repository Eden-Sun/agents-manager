# Rust 建置引擎與快取

## Blaze 開發建置

`scripts/project-cargo.sh` 是本專案開發用的 Cargo 入口，預設 `AGM_RUST_ENGINE=auto`。受管 Cargo shim 會往目前目錄的祖先找 `.cargo/agm-blaze` 與這支腳本，兩者同時存在才轉交；其他專案不受影響。轉交發生在拿名額之前，再以 `cargo blaze …`（舊版 release 用 `cargo ra-jit …`）進入既有 shim，仍只租一個建置名額，不直接呼叫加速器繞過排程。`cargo dev` 的 xtask 建 daemon 也使用此入口。daemon 換版後新寫入／刷新過的 shim 才會有自動轉交；換版前或非受管 shell 直接使用腳本。

```sh
scripts/install-blaze.sh
scripts/project-cargo.sh build -p agents-managerd
scripts/project-cargo.sh run -p agents-managerd -- serve
scripts/project-cargo.sh test -p xtask
AGM_RUST_ENGINE=cargo scripts/project-cargo.sh build -p agents-managerd
```

安裝器固定上游 installer 的 commit，下載的 release 依上游 SHA256SUMS 校驗。工具放 `~/.local/share/agents-manager/blaze/bin`，不寫使用者 Cargo 設定、不執行 `blaze enable`；`AGM_BLAZE_INSTALL_HOME` 可改安裝位置，執行時以 `AGM_BLAZE_BIN` 指到對應 bin。入口把這個目錄加在 PATH 尾端，不蓋掉受管 Cargo shim。安裝器可能下載 Rust 1.99.0 與 rust-src；入口要求這個工具鏈並指定 `RUSTUP_TOOLCHAIN=1.99.0`，使 rustc 輸出與 Blaze 的 ABI 一致，不以其他版本的產物拼接。升級 Blaze 時需重新核對其工具鏈要求。

| 路徑 | 執行方式 |
|---|---|
| 預設 profile 的 `build`／`run`／`test` | Blaze；完整保留參數與退出碼 |
| `--release`／`-r`、顯式 `--profile`／`--target`、JSON 診斷、`-Z` | 原生 Cargo |
| `check`、Clippy、fmt、顯式 `+toolchain`、其他 Cargo 子指令 | 原生 Cargo |
| `scripts/check.sh`、Ubuntu CI、macOS 原生驗證、正式打包 | 原生 Cargo／rustc，作為最終驗證 |

`auto` 在不支援的主機、缺工具或缺 Rust 1.99.0 時明講並回到 Cargo；`AGM_RUST_ENGINE=blaze` 遇到這些條件則失敗，方便確認實際有走加速器。支援主機是 Linux x86-64／aarch64 與 macOS arm64。編譯或測試已執行而失敗時，入口不重試成另一條成功路徑。Blaze 自身的相容性 fallback 由上游管理；看到 `[ra-jit]` 才表示該輪由引擎建置。`test` 強制 `BLAZE_TESTS=all`／`RA_JIT_TESTS=all`，不採只跑受影響測試的模式；測試執行緒預設 8，可由 `RUST_TEST_THREADS` 覆寫。普通原生 Cargo 的遠端驗證規則保留；Blaze 指令保留在呼叫主機，以重用當地記憶體快取。

Blaze 把分析資料與機器碼留在每個使用者共用的背景服務；首次建置、依賴／manifest／建置輸入變動仍可能走 Cargo，局部函式修改才適合快路徑。本專案 daemon 的 `build.rs` 會輸出 commit SHA 與髒樹標記；切換 commit 或 clean→dirty 改變這些輸出時，Blaze 會要求 Cargo 重建再重新載入，所以不能把小型 xtask 的加速倍數套到每一輪 daemon 建置。每個 target dir 照舊獨立，既有 sccache wrapper 照舊處理 rustc 呼叫。背景 worker 可吃數 GiB，且命令結束後仍存在；上游預設閒置 30 分鐘退出，worker 總記憶體預算為主機／cgroup 記憶體的一半。`RA_JIT_IDLE_SECS`、`RA_JIT_MEMORY_MAX` 可調，服務啟動時讀取；上游預算並非正在載入的 worker 的硬上限。不要把這層記憶體當成 Cargo 名額結束後就會釋放。

可用 `~/.local/share/agents-manager/blaze/bin/ra-jit status` 查本次 release 的 worker 與 fallback；新版另有 `blaze` 入口。背景服務為同一使用者共用，停止前先協調其他 agent，遵守只終止自己記錄的 PID 的行程規則。遇到產物／診斷差異，以 `AGM_RUST_ENGINE=cargo` 重現，最終以 `scripts/check.sh changed` 與 CI 結果為準。

## sccache 相依快取

`.cargo/config.toml` 的 `build.rustc-wrapper` 指到 `scripts/sccache-wrapper.sh`。這一層與 Blaze 的記憶體編譯快取不同：它快取 rustc 呼叫的產物，不能省掉 Cargo 本身的調度與連結。

- 帶 `-C incremental=…` 或 `-Cincremental=…` 的 workspace crate 原樣交給 rustc，不經過 sccache，也不做 server 探測，保留同一工作樹內的 Cargo 增量編譯。
- 其餘呼叫在 sccache 已安裝且 `--show-stats` 探測成功時使用快取；缺工具或探測失敗就原樣執行 rustc。真正的編譯錯誤仍原樣傳回。
- 預設 `SCCACHE_DIR=~/.cache/agents-manager-sccache`、`SCCACHE_CACHE_SIZE=10G`，呼叫端可覆寫。每台主機的工作樹共用此快取，各自的 `CARGO_TARGET_DIR` 與 Cargo 建置名額保持獨立。
- 不假設換工作樹就會命中；Rust 的產物與建置輸入含有路徑差異，實際收益以 `sccache --show-stats` 為準，不共用或手動搬移不同設定的 target 產物。

```text
開發 build/run/test → project-cargo → 受管 Cargo shim／名額 → Blaze
                                                         └ 首次／相容性 fallback → Cargo → sccache wrapper → rustc
原生驗證／release → 受管 Cargo shim／既有遠端流程 → Cargo → sccache wrapper → rustc
```

`bash scripts/project-cargo_test.sh` 驗證入口選擇、參數與錯誤傳遞；`bash scripts/sccache-wrapper_test.sh` 驗證相依快取的退回與增量行為。Cargo shim 的原生測試另外驗證 Blaze 只租用一個名額，以及測試執行緒不能超過名額上限；Mac 收尾入口見 [AGENT-VERIFY.md](AGENT-VERIFY.md)。
