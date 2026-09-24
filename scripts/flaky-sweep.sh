#!/bin/bash
# 一次抓出「整樹高負載下偶發紅」的一族：把 daemon 整樹測試在高並行下連跑 N 輪，可同時開多份製造負載，
# 最後彙總「任何一輪紅過的測試」與各自紅了幾輪。單跑都綠、整樹才紅的那種（#255／#274 一族）靠這支抓。
#
# ⚠ **本機預設禁跑**（協調者 2026-09-24 裁示，當天本機 load 壓到 85）：這支三件事都踩到那條禁令——
#   私有 `CARGO_TARGET_DIR`（繞過 cargo shim ＝不佔建置名額、也不會被轉到外部編譯主機）、
#   `--test-threads` 遠高於預設、同時開好幾份。要驗 flaky 請走遠端編譯主機或 CI。
#   真的要在本機跑，**只能在命令列加 `--i-know`** 明確同意，並先跟協調者講一聲——
#   刻意不留環境變數的繞法：那種開關會被 export 到整個 pane、或寫進某支腳本就默默一直開著，
#   「本機預設禁跑」就名存實亡，而且事後從指令看不出來當時到底有沒有同意。
#
# 用法：scripts/flaky-sweep.sh --i-know [-n 輪數=5] [-c 同時幾份=2] [-t RUST_TEST_THREADS=64] [-o 輸出目錄] [-f 測試名過濾] [-k]
#   -k  跳過編譯，直接用上次的測試 binary（改了程式要重編才會反映）
# 輸出：<輸出目錄>/<份>-<輪>.log 是每一輪的完整輸出；最後列出清單（紅過的測試、紅了幾輪／總輪數、第一個 panic 訊息）。
# 結束碼：0 全綠、1 有輪次紅、2 根本沒跑起來（參數不是正整數、編譯失敗、找不到 binary、一輪都沒跑到、沒有同意閘門）。
#   **「一輪都沒跑」絕對不會回 0**（issue #479）：這支的用途就是拿數字當證據，假綠最傷。
set -euo pipefail
ROUNDS=5 COPIES=2 THREADS=64 OUT="" FILTER="" SKIP_BUILD=0
# 檔頭就是說明書：印它而不是寫死行號——行號會隨著補充說明而失準，印出半段或印到程式碼。
usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" >&2; }
# 同意閘門：只有命令列的 `--i-know`（見檔頭）。getopts 不吃長選項，先自己撿掉。
AGREED=0
ARGS=()
for a in "$@"; do
  if [ "$a" = "--i-know" ]; then AGREED=1; else ARGS+=("$a"); fi
done
set -- ${ARGS+"${ARGS[@]}"}
if [ "$AGREED" != 1 ]; then
  usage
  echo "拒絕：本機預設禁跑（見上）。要跑請在命令列加 --i-know，或改走遠端編譯主機／CI。" >&2
  exit 2
fi
while getopts "n:c:t:o:f:k" o; do
  case $o in
    n) ROUNDS=$OPTARG ;; c) COPIES=$OPTARG ;; t) THREADS=$OPTARG ;; o) OUT=$OPTARG ;; f) FILTER=$OPTARG ;; k) SKIP_BUILD=1 ;;
    *) usage; exit 2 ;;
  esac
done
# 輪數／份數／執行緒要是正整數：`-n 0` 以前會讓 WANT=0、TOTAL=0，守門判定相等就 exit 0——
# 一輪都沒跑卻回全綠，正是這張票要擋的假綠；非數字則會在 seq／$((…)) 那裡爆成看不懂的錯（issue #479）。
pos_int() { # pos_int <旗標> <值>
  case "$2" in ''|*[!0-9]*) echo "$1 要是正整數，拿到的是 '$2'" >&2; exit 2 ;; esac
  [ "$2" -gt 0 ] || { echo "$1 要大於 0，拿到的是 '$2'" >&2; exit 2; }
}
pos_int -n "$ROUNDS"
pos_int -c "$COPIES"
pos_int -t "$THREADS"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/flaky-sweep.XXXXXX")}
mkdir -p "$OUT" || { echo "建不出輸出目錄 $OUT" >&2; exit 2; }
export CARGO_TARGET_DIR=${FLAKY_TARGET_DIR:-$ROOT/target/flaky-sweep}
cd "$ROOT" || exit 2

if [ "$SKIP_BUILD" = 0 ]; then
  echo "編譯測試 binary…" >&2
  cargo test -p agents-managerd --no-run 2>"$OUT/build.log" || { tail -20 "$OUT/build.log"; exit 2; }
fi
# `|| true`：pipefail 下 cargo 失敗會讓整條管線非 0，`set -e` 會在下面那句友善訊息之前就把腳本殺掉。
BIN=$(cargo test -p agents-managerd --no-run --message-format=json 2>/dev/null \
  | sed -n 's/.*"executable":"\([^"]*agents_managerd-[^"]*\)".*/\1/p' | tail -1 || true)
[ -x "$BIN" ] || { echo "找不到測試 binary（試試不加 -k）" >&2; exit 2; }
echo "binary: $BIN  輪數=$ROUNDS 同時=$COPIES threads=$THREADS  輸出: $OUT" >&2

run_copy() { # $1=份
  # `cd` 失敗以前只是 `return`，父行程看不到，那一份就靜靜地一輪都沒跑（issue #479）。
  cd "$ROOT/daemon" || {
    echo "第 $1 份：進不去 $ROOT/daemon，這一份一輪都沒跑" >&2
    echo "$1 0 97" >>"$OUT/rounds.txt"; return 2
  }
  for r in $(seq 1 "$ROUNDS"); do
    rc=0
    "$BIN" --test-threads="$THREADS" $FILTER >"$OUT/$1-$r.log" 2>&1 || rc=$?
    echo "$1 $r $rc" >>"$OUT/rounds.txt"
  done
}
: >"$OUT/rounds.txt"
pids=()
for c in $(seq 1 "$COPIES"); do run_copy "$c" & pids+=("$!"); done
# 每一份的 rc 都要收：`wait` 不看各自的結束碼，漏掉就等於「沒跑起來」跟「跑完全綠」同一個結果。
copy_bad=0
for pid in "${pids[@]}"; do wait "$pid" || copy_bad=1; done

TOTAL=$(wc -l <"$OUT/rounds.txt" | tr -d ' ')
BAD=$(awk '$3!=0' "$OUT/rounds.txt" | wc -l | tr -d ' ')
echo
echo "共 $TOTAL 輪，紅了 $BAD 輪"
# 「一輪都沒跑到」「少跑了幾輪」都不能算證據（issue #479）：以前 TOTAL=0 會讓 BAD=0，
# 最後那行 `[ "$BAD" = 0 ]` 成立，整支 exit 0——跟「跑滿 10 輪全綠」給出同一個結束碼。
WANT=$((ROUNDS * COPIES))
if [ "$TOTAL" -ne "$WANT" ] || [ "$copy_bad" != 0 ]; then
  echo "只跑到 $TOTAL/$WANT 輪（有份沒跑起來？見 $OUT/rounds.txt 與 $OUT/build.log）——這輪不算數" >&2
  exit 2
fi
# 每個紅過的測試：紅了幾輪（同一輪只算一次）＋第一個 panic 訊息。
# 全綠時 grep 找不到東西會回 1，pipefail＋set -e 就會在**最該回 0 的那一刻**把腳本殺掉。
# 整段各自 `|| true`，讓結束碼只由最後那行 `[ "$BAD" = 0 ]` 決定。
grep -h "^test .* \.\.\. FAILED$" "$OUT"/*-*.log 2>/dev/null | sed 's/^test \(.*\) \.\.\. FAILED$/\1/' | sort | uniq -c | sort -rn | while read -r n name; do
  first=$(grep -l "^test $name \.\.\. FAILED$" "$OUT"/*-*.log 2>/dev/null | head -1 || true)
  msg=$(grep -h -A3 "^thread '$name'" "$first" 2>/dev/null | sed -n '2,4p' | tr '\n' ' ' | cut -c1-200 || true)
  printf '%3d/%s  %s\n      %s\n' "$n" "$TOTAL" "$name" "$msg"
done || true
[ "$BAD" = 0 ]
