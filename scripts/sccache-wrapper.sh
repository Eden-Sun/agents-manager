#!/usr/bin/env bash
# `.cargo/config.toml` 的 `build.rustc-wrapper` 指到這裡，不是直接指 `sccache`（issue #91）：
# 這樣沒裝 sccache 的機器、或這台機器的 sccache 忽然壞掉，cargo 一樣編得過，只是少了快取。
# cargo 呼叫的形狀是 `<wrapper> <rustc 本身> <rustc 的參數...>`，所以「什麼都不做」時只要
# 原封不動 exec 剩下的參數即可。
#
# 快取目錄跟大小上限預設在這裡給，caller（例如 cargo-slot.sh）沒設過才生效，好讓每個人
# 不用另外記得設：同一台機器上所有 managed worktree 共用同一個 sccache server／同一份快取，
# 這樣才吃得到「別的 worktree 剛編過同一份沒改動的依賴」這個好處；每個 worktree 自己的
# `CARGO_TARGET_DIR` 完全不動，快取只是省下重算 rustc 的輸出，不共用 target/ 本身。
set -euo pipefail

# 只有「sccache 自己健康」才走它：先問一次 --show-stats（server 沒起會順便拉起來），失敗就當作
# sccache 壞了、退回直編。探測放在編譯之前，所以真正的編譯錯誤（sccache 原樣轉出 rustc 的結束碼）
# 不會被誤判成 sccache 壞掉、也不會被吞成成功。探測過了之後 sccache 在編譯途中才掛掉的情況不在
# 這裡處理（sccache 自己對快取後端錯誤會退回本機編譯）。
# 探測要吃跟真正編譯同一組 SCCACHE_DIR／大小，所以預設值先給。
: "${SCCACHE_DIR:="${HOME:-/tmp}/.cache/agents-manager-sccache"}"
: "${SCCACHE_CACHE_SIZE:=10G}"
export SCCACHE_DIR SCCACHE_CACHE_SIZE
if command -v sccache >/dev/null 2>&1 && sccache --show-stats >/dev/null 2>&1; then
  # sccache 不支援 incremental compilation（官方文件的已知限制：incremental 的產物本質上跟
  # 這次編譯自己的歷史狀態綁在一起，沒辦法變成能重用的快取鍵）。cargo 給每個 crate 的
  # `-C incremental=<CARGO_TARGET_DIR>/…` 路徑天生跟著 target dir 走，每個 worktree 都不一樣；
  # 帶著它送給 sccache，本來完全沒改過的依賴也會因為路徑不同被算成不同的 key，實測命中率是 0%
  # （issue #91 量測記錄）。過濾掉這個旗標再交給 sccache，key 只跟著「編什麼」走，不跟著
  # 「編到哪個 target dir」走；沒裝 sccache 的退化路徑完全不受影響——這段過濾只在這個 if 裡面，
  # 原生 rustc 照樣拿到 cargo 給的完整旗標，同一個 worktree 內的 incremental 照常運作。
  filtered=()
  args=("$@")
  n=${#args[@]}
  i=0
  while [ "$i" -lt "$n" ]; do
    a="${args[$i]}"
    if [ "$a" = "-C" ] && [ $((i + 1)) -lt "$n" ]; then
      case "${args[$((i + 1))]}" in
        incremental=*) i=$((i + 2)); continue ;;
      esac
    fi
    case "$a" in
      -Cincremental=*) i=$((i + 1)); continue ;;
    esac
    filtered+=("$a")
    i=$((i + 1))
  done
  exec sccache "${filtered[@]}"
fi
exec "$@"
