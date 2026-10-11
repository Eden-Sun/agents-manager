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
# 這裡處理（sccache 自己對快取後端錯誤會退回本機編譯）。incremental 的 workspace crate 不經過 sccache，
# 連探測都不做。
# 探測要吃跟真正編譯同一組 SCCACHE_DIR／大小，所以預設值先給。
: "${SCCACHE_DIR:="${HOME:-/tmp}/.cache/agents-manager-sccache"}"
: "${SCCACHE_CACHE_SIZE:=10G}"
export SCCACHE_DIR SCCACHE_CACHE_SIZE
# workspace crate（cargo 給的 `-C incremental=<CARGO_TARGET_DIR>/…`）不經過 sccache：sccache 不支援
# incremental compilation，經過它會把這幾顆 crate 變成每次都非增量的整顆重編（#1440）。這個判斷放在
# `--show-stats` 探測之前，所以 workspace crate 連探測都省掉。兩種寫法都認：`-C incremental=…`（兩個
# token）與 `-Cincremental=…`（單一 token）。
# 沒有這個旗標的（registry 依賴，本來就沒有 incremental）才走 sccache，旗標原樣交給它、不做任何濾除。
incremental=0
prev=""
for a in "$@"; do
  case "$a" in
    -Cincremental=*) incremental=1 ;;
    incremental=*) [ "$prev" = "-C" ] && incremental=1 ;;
  esac
  prev="$a"
done
if [ "$incremental" = 0 ] && command -v sccache >/dev/null 2>&1 && sccache --show-stats >/dev/null 2>&1; then
  exec sccache "$@"
fi
exec "$@"
