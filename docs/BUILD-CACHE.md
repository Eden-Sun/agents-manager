# 共用編譯快取（sccache，issue #91）

多顆 agent 各自一個 worktree、各自一個 `CARGO_TARGET_DIR`（見 `CLAUDE.md`「開工前」），是刻意的：
避免大家搶同一份 `target/` 的鎖、也讓半成品互不干擾。代價是同樣沒改過的依賴（`axum`、`sqlx`、
`tokio`…）在每個 worktree 各編一次，而這台機器的 rustc 併發又被壓到 2（`scripts/cargo-slot.sh`），
編譯常常是整條產線的瓶頸。

sccache 理論上補的是這一塊：它快取「rustc 這一次呼叫的輸出」，用內容雜湊當 key，跟輸出放在哪個
`target/` 目錄無關。**第二輪（見下面「第二輪：root cause」）已經把命中率接近 0% 的原因查到底
了——結論是負面的**：sccache 0.18.0 的 Rust 快取路徑跟這個專案「每個 worktree 各自
`CARGO_TARGET_DIR`」的架構本質衝突，不是設定沒調對，官方文件建議的解法（`SCCACHE_BASEDIRS`）
在 Rust 這條路徑上也沒有實作，換 target dir 之後不太可能拿到跨 worktree 的命中。已經確定有效、
值得先合進 main 的是「wrapper 骨架＋不硬性依賴」這件事本身（見「怎麼接上去的」）——這部分完全
不受這個結論影響，繼續留著當安全網。

## 怎麼接上去的

`.cargo/config.toml` 的 `build.rustc-wrapper` 指到 `scripts/sccache-wrapper.sh`，不是直接指
`sccache` 本身：

```
cargo → rustc-wrapper（scripts/sccache-wrapper.sh）→ 有裝 sccache？ → sccache → rustc
                                                    └ 沒裝／壞掉？ → 直接呼叫 rustc（原樣退化）
```

- **沒裝 sccache 的機器完全不受影響**：wrapper 找不到 `sccache` 執行檔就直接 `exec` 剩下的參數
  （也就是原本那句 rustc 呼叫），編譯照常進行，只是沒有跨 worktree 的快取。這是刻意設計成
  「非硬性依賴」——`.cargo/config.toml` 一旦合進 main，所有 worktree（含還沒裝 sccache 的
  遠端主機）下一次 `git pull` 都會拿到這個設定，不能因此編不過。
- **`CARGO_TARGET_DIR` 完全不動**：每個 worktree／每顆 agent 照舊各自一份，`cargo-slot.sh` 的
  排隊名額機制也完全沒改——快取是正交的一層，接在 rustc 前面，不影響「誰能開始編」這件事。
- **不會跨不相容設定汙染**：key 由 sccache 自己算（rustc 版本、target triple、`--cfg`/features、
  原始碼雜湊……都在裡面），這裡沒有另外寫任何手動判斷「這兩次編譯算不算一樣」的邏輯，全部信任
  sccache 自己的 key，也不做旁路的「直接複製 artifact」這種捷徑。
- **轉呼叫 sccache 前先濾掉 `-C incremental=<path>`**（兩個 token `-C incremental=…` 與單一 token
  `-Cincremental=…` 兩種寫法都濾）：sccache 官方文件明講不支援 incremental compilation；cargo 給
  workspace 自己那幾個 crate 的 `-C incremental=<CARGO_TARGET_DIR>/…` 路徑天生跟著 target dir 走，
  每個 worktree 都不一樣，帶著它送給 sccache 只會讓那幾個 crate 的 key 永遠對不上。這段過濾**只在
  `sccache` 存在這個分支裡**：沒裝 sccache 時原封不動照舊，同一個 worktree 內的 incremental
  compilation 不受影響。第三方依賴（`~/.cargo/registry` 底下的 crate）本來就沒有這個旗標，不在
  這條修正的範圍內——它們命中率低是另一個還沒解開的問題，見下面。

## 現況與量到的數字（2026-09-18，issue #91 第一輪）

**结論先講**：wrapper 骨架本身正確、安全、零成本退化，已經合進 main；但**這個 repo 目前量到的
sccache 命中率接近 0%，還沒有拿到「兩個 worktree 共用快取」這個目標效果**。原因沒有在這一輪完全
釐清，寫在這裡留給下一輪，不要假裝已經解決。

量測方法：同一份原始碼（沒有任何改動）、`cargo build -p agents-managerd`（`CARGO_BUILD_JOBS=1`，
跟 `cargo-slot.sh` 一致），三段分開量：

| 階段 | CARGO_TARGET_DIR | sccache | cargo 自己回報的編譯時間 |
|---|---|---|---|
| baseline | 全新、獨立 | 沒接（`.cargo/config.toml` 暫時還原成沒有 `rustc-wrapper`） | **2m29s** |
| cold（第一次填快取） | 全新、獨立 | 有接，快取目錄全新 | **2m36s** |
| warm（模擬第二個 worktree，同一份快取） | 另一個全新、獨立 | 有接，沿用 cold 那份快取 | **2m18～2m20s** |

（`time` 量到的 real time 還含排隊等 `cargo-slot.sh` 名額的時間，跟其他子 agent 搶名額嚴重時
可以到 8 分鐘以上——那是這台機器目前的名額壓力，不是 sccache 的效果，只看 cargo 自己印的
`Finished ... in` 那行。）

三段時間彼此在誤差範圍內，**warm 沒有比 cold 明顯快**——`sccache --show-stats` 直接證實了這件事：
同一個 sccache server session 裡，cold 跑完（0/245 命中）之後緊接著跑 warm，一樣是 0/245 命中；
另外用已經跑過 cold＋warm、快取確定是熱的那份 `SCCACHE_DIR` 再重跑一次（新的 server session），
也只有 1/247 命中（命中率 0.41%）。

**已經排除的原因**：
- **不是 `--out-dir` 或 `-L dependency=<target-dir>/…` 本身**——這兩個旗標天生跟著 target dir
  走，但用一支不經過 cargo、單獨呼叫 `sccache rustc` 的最小重現（一個沒有依賴的 `lib` crate，
  只有 `--out-dir`／`-L dependency=` 不同，其他完全一樣）驗證過：sccache 照樣算出同一把 key、
  照樣命中。所以 sccache 本身確實有把這兩個旗標正規化掉。
- **不是 `-C metadata=…` / `-C extra-filename=…` 隨 target dir 變**——直接用 `cargo build -v`
  比對同一顆 crate（`xtask`，故意選零依賴的，排除依賴圖差異）在兩個不同 `CARGO_TARGET_DIR` 下的
  完整 rustc 呼叫，兩邊的 `-C metadata=`／`-C extra-filename=` 完全一樣。
- **`-C incremental=<path>` 濾掉後確實生效**——用一份加了除錯輸出的 wrapper 複本重跑過一次，
  確認送進 `sccache` 的參數裡真的已經沒有這個旗標；但即使濾乾淨，實際 build 的整體命中率仍然
  接近 0%，代表這不是（或不是唯一的）主要原因。

**還沒排除、留給下一輪的懷疑**：
- 有 `build.rs` 的 crate（`libsqlite3-sys`、`ring`、`zerocopy`…）很可能透過
  `include!(concat!(env!("OUT_DIR"), …))` 這類手法把 `OUT_DIR`（天生是 target-dir 底下的路徑）
  燒進編譯輸出本身，這種情況下跨 target dir 本來就**不該**命中，算是正確行為而不是 bug——但目前
  沒有量過「扣掉這些 crate 之後，剩下單純的 `lib` 依賴（像 `once_cell`）命中率是多少」。
- 直接觀察到的反例：`once_cell`（沒有 build.rs、也沒有 `-C incremental`）在最小重現裡命中，
  但在完整專案的實際 build 裡沒有命中，兩者的差異目前還沒有用 `SCCACHE_LOG=debug` 之類的工具
  逐項比對出來（會需要再跑一輪完整 build，這一輪的名額配額已經用完，留給下一輪）。
- cargo 或 sccache 是否把 `cargo-slot.sh` 設的 `CARGO_BUILD_JOBS=1`、或其他環境變數（`PWD`、
  子 agent 各自不同的環境）意外編進了 cache key，也還沒排除。

**建議**：下一輪先用 `SCCACHE_LOG=debug SCCACHE_ERROR_LOG=/tmp/sccache.log` 重跑一次 cold／warm
兩輪，直接看 sccache 自己記錄的 hash 輸入，而不是像這一輪一樣用排除法猜。

## 第二輪：root cause 確認（2026-09-18）

**结論先講：查到底了，是負面結論**——sccache 0.18.0 對 Rust 的快取路徑跟「每個 worktree 各自
`CARGO_TARGET_DIR`」這個架構前提衝突，不是這個 repo 設定沒調好，官方文件建議的正規化解法在
Rust 這條路徑上也沒有實作。第一輪「不是 `--out-dir`／`-L dependency=` 本身」那條排除，**這一輪
推翻**：第一輪的最小重現手寫了一個簡化過的 `sccache rustc` 呼叫，形狀跟 cargo 真正產生的呼叫
對不起來，才會誤判成「有正規化」。

**方法**：不再對整個 `agents-managerd` 重跑 cold／warm（吃 cargo 名額，量測雜訊也大），改用一顆
兩個檔案、零依賴風險的最小 crate（`oncetest`，只依賴 `once_cell = "=1.21.4"`，跟 `Cargo.lock`
釘的版本一致，沒有 `build.rs`），接上這個 repo 真正的 `scripts/sccache-wrapper.sh`，透過
`SCCACHE_SERVER_PORT` 開一個獨立的 sccache server／`SCCACHE_DIR`（不影響其他 agent 正在用的那個
共用 server），`SCCACHE_LOG=debug SCCACHE_ERROR_LOG=...` 直接看 sccache 記錄的 hash key 與參數。
真正吃 cargo 名額的呼叫一樣全部走 `cargo-slot.sh`。

**查到的第一個坑（測試方法論，不是 sccache 的問題）**：一開始用 `--manifest-path` 指到
`oncetest/Cargo.toml`、但從別的目錄呼叫 `cargo-slot.sh`，wrapper 完全沒被呼叫到（`sccache
--show-stats` 掛的請求數是 0）。原因：**cargo 找 `.cargo/config.toml` 是照「呼叫當下的目前工作
目錄」往上找，不是照 `--manifest-path` 所在的目錄**——這個坑也可能影響到過去派工指令裡「沒有先
`cd` 進 worktree、只給 `CARGO_TARGET_DIR` 就直接呼叫 `cargo-slot.sh`」的呼叫，值得所有子 agent
往後留意：呼叫 `cargo-slot.sh` 前先 `cd` 進那個 worktree 的目錄，不要只給 `--manifest-path`。
改成先 `cd` 進 crate 目錄之後，wrapper 才真正被呼叫、sccache 才真的介入。

**核心證據**：修好呼叫方式之後，`once_cell`（零依賴、沒有 `build.rs`）這種最單純的情況：
- **同一個 `CARGO_TARGET_DIR` 重編一次**（其他都不動）→ **命中**（2/2）。
- **换成另一個 `CARGO_TARGET_DIR` 重編一次**（原始碼、`SCCACHE_DIR`、sccache server session
  都不動，只換 target dir）→ **完全沒中**（0/2）。

直接比對 `SCCACHE_LOG=debug` 印出來的兩次 `once_cell` `Hash key:`，兩把 key 不一樣，而 cargo
產生的完整 rustc 參數列表裡，兩次唯一的差異就是 `--out-dir`／`-L dependency=`（值跟著
`CARGO_TARGET_DIR` 走）——`-C metadata=`／`-C extra-filename=` 兩次完全一樣（跟第一輪的排除
結果一致）。這證明：**只要 `CARGO_TARGET_DIR` 换掉，這個 sccache 版本對 Rust 編譯的雜湊鍵就會
換，不需要任何 `build.rs`／`OUT_DIR` 涉入就會發生**——第一輪懷疑的「有 `build.rs` 的 crate 才會
不命中」不是主要原因，連最單純的葉節點依賴都不命中。

**追查機制、但沒有完全對上**：查了 sccache 上游原始碼（`src/compiler/rust.rs`，
`generate_hash_key`）。原始碼註解明講 `-L`／`--extern`／`--out-dir` 這幾個旗標**本身**會被排除在
雜湊之外（換句話說，官方設計是「不管路徑字串，只雜湊路徑指到的檔案內容」），但同一段
`generate_hash_key` 也明講會把**「這次編譯的 cwd」**放進雜湊（原文大意：cwd 會跑進編出來的
rlib 裡）。這一輪的重現裡，`once_cell` 是從 `~/.cargo/registry` 讀出來編的、`oncetest` 是從固定
的 crate 目錄編的，兩次呼叫的作業系統層級 cwd 照理說不會因為换了 `CARGO_TARGET_DIR` 而變──但
實際命中率就是變了。**這一輪沒能在剩下的時間內把「原始碼講的排除清單」跟「實測觀察到的雜湊鍵
變化」完全對上**，可能是這個雜湊還吃了什麼還沒找到的間接輸入（例如 dep-info 裡記的路徑、或
`--out-dir`／`-L dependency=` 雖然字串本身被排除、但透過某個間接管道還是讓 hash 內容跟著變）。
**誠實記錄這個沒對上的地方，不假裝已經完全弄懂內部機制**——但下面這個事實已經用直接對照
`Hash key:` 與可重複的 A/B 測試釘住，不受這個機制細節影響。

**官方建議的解法測過了，沒用**：sccache 文件建議用 `SCCACHE_BASEDIRS`（`:` 分隔的絕對路徑清單，
把落在清單裡任一目錄底下的路徑，雜湊前先換算成相對路徑，讓不同機器/不同目錄結構的建置也能對上
同一把 key）解決這類問題。這一輪測過：
1. `strings` 直接查已安裝的 `sccache` 執行檔，確認 `SCCACHE_BASEDIRS` 這個字串真的在二進位裡
   （不是編譯掉的功能）。
2. 重啟一份獨立的 debug sccache server（不影響其他 agent 正在用的共用 server），啟動時就把
   `oncetest` 用到的兩個 `CARGO_TARGET_DIR` 都塞進 `SCCACHE_BASEDIRS`（環境變數是伺服器啟動時
   讀的設定，不是每個請求各自帶的，這裡有先重開伺服器確保設定生效）。
3. 結果：**還是 0 命中**，跟沒設 `SCCACHE_BASEDIRS` 一樣。

再查 `sccache` 二進位裡 `SCCACHE_BASEDIRS` 附近的字串，找到關鍵字：`"Stripping basedirs from
preprocessor output with length "`——**這個正規化是接在「preprocessor 快取模式」（C/C++ 那條路）
上的，不是 Rust 這條完全不同的雜湊路徑**。直接抓 sccache 上游 `src/compiler/rust.rs` 原始碼比對
過，裡面完全沒有任何 `basedir` 相關的呼叫。這就是「照文件設定了，卻沒有效果」的原因：**這個版本
的 sccache 對 Rust 編譯根本沒有實作 basedir 正規化**，不是我們設定錯。

**結論（可以拿去用的部分）**：
- 這個 repo「每個 worktree 各自 `CARGO_TARGET_DIR`」的架構要求，跟 sccache 0.18.0 對 Rust 編譯
  的雜湊鍵設計互斥——換 target dir 幾乎保證雜湊鍵跟著換，不管專案多大、有沒有 `build.rs`，連
  最單純的零依賴葉節點都一樣。這不是「調參數就能修好」的問題，是這個版本的 sccache 對 Rust 的
  已知限制（官方的路徑正規化解法只覆蓋 C/C++ 路徑）。
- issue #91 的驗收條件之一「兩個管理中的 worktree 編相同未改動依賴能拿到快取命中」**在目前的
  sccache 版本、目前的架構要求下判定為做不到**，不是還沒查出來，建議把這條標成「已知不可行」
  而不是繼續調參數重跑量測。
- Wrapper 骨架（`.cargo/config.toml` 指到 `scripts/sccache-wrapper.sh`、沒裝 sccache 就原生
  退化、濾掉 `-C incremental=`）本身正確、安全、零成本退化，這個結論不影響它繼續留著；只是
  「跨 worktree 命中」這個目標效果，不建議再花時間追。
- 沒有評估過、也不在這次範圍內的下一步（如果之後還要追這個目標）：換一個對 Rust 有實作
  basedir/路徑正規化的編譯快取工具、或重新考慮「每個 worktree 各自 target dir」這個前提本身。

## 快取位置與大小上限

預設 `~/.cache/agents-manager-sccache`，上限 `10G`（sccache 自己做 LRU 淘汰，這裡不用另外寫
清理腳本）。想換位置或大小，在跑 cargo 之前設環境變數即可，wrapper 只在**沒人設過**時才補預設，
不會蓋掉：

```sh
export SCCACHE_DIR=/some/bigger/disk/sccache
export SCCACHE_CACHE_SIZE=30G
```

## 診斷：現在有沒有在用、命中率多少

```sh
bash scripts/sccache-status.sh
```

沒裝 sccache 時印一句「沒裝」加安裝提示就結束（exit 0，不是錯誤）；裝了就印 `SCCACHE_DIR` 與
`sccache --show-stats`（含 request 數、cache hits/misses、快取目前用量）。

## 遠端主機

遠端 build host 一樣照這個機制：只要那台機器裝了 `sccache`、PATH 找得到，`.cargo/config.toml`
一併帶過去（本來就是 repo 的一部分）就會自動生效；沒裝就自動退化成原生 rustc，不需要另外開關。
多顆 agent 共用同一台遠端主機時，`SCCACHE_DIR` 預設也是那台主機自己的 `~/.cache/...`，一樣是
主機層級共用，不是每個 worktree 各一份。

## 工具鏈升級的影響

sccache 的 cache key 含 rustc 版本／commit hash，所以：

- **升級 rustc／toolchain 後，舊快取自然不會被誤用**——key 對不上就是 cache miss，退化成正常編譯
  （較慢的第一次），不會拿舊工具鏈編出來的東西冒充新工具鏈的結果。
- **舊版本的快取條目不會自動被清掉**，只會在 `SCCACHE_CACHE_SIZE` 頂到上限時被 LRU 淘汰掉。長期
  只用單一 toolchain 的機器不用管；常常換 toolchain 版本測試的機器，快取的「有效命中比例」會下降
  （因為總量裡混了多個版本的條目），可以調大 `SCCACHE_CACHE_SIZE` 或手動清空
  `$SCCACHE_DIR` 重新開始。

## 測試

`scripts/sccache-wrapper_test.sh`（`bash scripts/sccache-wrapper_test.sh`）：隔離測試，PATH 鎖進
暫時目錄，兩條路徑（裝了／沒裝 sccache）都覆蓋，不受跑測試的機器現況影響；釘住「沒裝時原樣呼叫
rustc（含 incremental 旗標不受影響）」「裝了轉呼叫 sccache 而不是繞過它」「兩種寫法的
`-C incremental=`／`-Cincremental=` 都被濾掉、其他 `-C` 旗標留著」「快取目錄／大小上限的預設與
不覆寫呼叫端設定」「結束碼原封不動回傳」，15 條全過。
