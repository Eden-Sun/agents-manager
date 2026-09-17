# 共用編譯快取（sccache，issue #91）

多顆 agent 各自一個 worktree、各自一個 `CARGO_TARGET_DIR`（見 `CLAUDE.md`「開工前」），是刻意的：
避免大家搶同一份 `target/` 的鎖、也讓半成品互不干擾。代價是同樣沒改過的依賴（`axum`、`sqlx`、
`tokio`…）在每個 worktree 各編一次，而這台機器的 rustc 併發又被壓到 2（`scripts/cargo-slot.sh`），
編譯常常是整條產線的瓶頸。

sccache 理論上補的是這一塊：它快取「rustc 這一次呼叫的輸出」，用內容雜湊當 key，跟輸出放在哪個
`target/` 目錄無關。**這份文件也誠實記錄了目前量到的結果：接上去之後，這個 repo 實測的命中率
還是接近 0%**——原因與已排除、未排除的可能性見下面「現況與量到的數字」。已經確定有效、值得先合進
main 的是「wrapper 骨架＋不硬性依賴」這件事本身（見「怎麼接上去的」），命中率的問題留給下一輪。

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
