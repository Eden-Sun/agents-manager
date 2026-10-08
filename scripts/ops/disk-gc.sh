#!/bin/bash
# 回收編譯快取（issue #835）：遠端編譯的 remote-cargo 快取、各 worktree 的 target/incremental 與久沒用的整個 target/。
#
# 根因：沒有任何排程回收磁碟；daemon 的 `select_gc` 只在 `[build.remote] enabled` 時才跑，worktree 的 target/ 更是越堆越多，
# 磁碟滿了才有人發現。由 systemd user timer `com.agm.disk-gc.timer` 每小時跑一次（Linux），純機械不花 LLM。
#
#   (a) ~/.cache/agents-manager/remote-cargo/* 頂層目錄超過 DISK_GC_REMOTE_CARGO_DAYS 天沒動、而且沒有行程在用 → 刪（不論 build.remote 開關）
#   (b) 主樹與各 worktree 的 target/*/incremental：超過 DISK_GC_INCREMENTAL_DAYS 天的子目錄 → 刪
#   (c) 各 worktree（不含主樹）的 target/：.fingerprint 超過 DISK_GC_TARGET_DAYS 天沒動 → 整個 target/ 刪
#   (d) 事後看 $HOME 所在磁碟：使用率 ≥ DISK_GC_ALERT_PCT 或剩餘 < DISK_GC_ALERT_FREE_GB → ops-alert disk_low
#
# 破壞性，所以：只刪上面這幾個明確路徑；路徑含 `..`、不在 $HOME 底下、任何一層是 symlink、實際位置跟預期不符，
# 就整步放棄（不擴大範圍）；cd 進去以後只用相對路徑刪。worktree 底下有行程的 cwd，或 target/ 底下有行程開著檔，整棵不動。
# 讀不到 /proc 就不做任何刪除（無法證明沒人在用）。
#
# 可覆寫（測試用）：AGM_REPO、DF_BIN、AGM_BIN、DISK_GC_LOG、DISK_GC_* 門檻。
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="${DISK_GC_LOG:-$DIR/disk-gc.log}"
AGM_REPO="${AGM_REPO:-$HOME/project/agents-manager}"
AGM="${AGM_BIN:-$DIR/bin/agm}"
DF="${DF_BIN:-df}"
REMOTE_DAYS="${DISK_GC_REMOTE_CARGO_DAYS:-7}"
INC_DAYS="${DISK_GC_INCREMENTAL_DAYS:-2}"
TARGET_DAYS="${DISK_GC_TARGET_DAYS:-7}"
ALERT_PCT="${DISK_GC_ALERT_PCT:-85}"
ALERT_FREE_GB="${DISK_GC_ALERT_FREE_GB:-20}"
for v in REMOTE_DAYS INC_DAYS TARGET_DAYS ALERT_PCT ALERT_FREE_GB; do
  case "${!v}" in ''|*[!0-9]*) echo "disk-gc: ${v} 不是數字（${!v}）" >&2; exit 2 ;; esac
done

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

HOME_REAL=$(cd "$HOME" 2>/dev/null && pwd -P) || { log "拒絕：無法解析 HOME"; exit 1; }
case "$HOME" in /*) ;; *) log "拒絕：HOME 不是絕對路徑"; exit 1 ;; esac

FREED_KB=0
SKIPPED=""
skip() { SKIPPED="${SKIPPED}${SKIPPED:+；}$1"; }

# 路徑要在 $HOME 底下、不含 `..`、從 $HOME 往下每一層都不是 symlink、而且實際位置跟字面一致。
safe_dir() { # safe_dir <絕對路徑>：通過回 0
  local p="${1%/}" rest cur comp
  case "$p" in *..*) return 1 ;; "$HOME"/*) ;; *) return 1 ;; esac
  rest="${p#"$HOME"/}"; cur="$HOME"
  while [ -n "$rest" ]; do
    comp="${rest%%/*}"
    [ -n "$comp" ] || return 1
    cur="$cur/$comp"
    [ ! -L "$cur" ] || return 1
    case "$rest" in */*) rest="${rest#*/}" ;; *) rest= ;; esac
  done
  [ -d "$p" ] || return 1
  [ "$(cd "$p" 2>/dev/null && pwd -P)" = "$HOME_REAL${p#"$HOME"}" ]
}

# 誰在用：行程的 cwd 與開著的檔案。收集一次，之後逐棵比對；讀不到 /proc 就整支不刪。
CWDS=$(mktemp "${TMPDIR:-/tmp}/disk-gc-cwd.XXXXXX") || exit 1
FDS=$(mktemp "${TMPDIR:-/tmp}/disk-gc-fd.XXXXXX") || exit 1
trap 'rm -f "$CWDS" "$FDS"' EXIT
if [ -r /proc/self/cwd ] || [ -L /proc/self/cwd ]; then
  for p in /proc/[0-9]*; do
    readlink "$p/cwd" 2>/dev/null
  done | grep '^/' > "$CWDS"
  for p in /proc/[0-9]*; do
    for f in "$p"/fd/*; do readlink "$f" 2>/dev/null; done
  done | grep '^/' > "$FDS"
  PROC_OK=1
else
  PROC_OK=0
fi
phys() { echo "$HOME_REAL${1#"$HOME"}"; }   # /proc 給的是實體路徑，比對前先換成實體
under() { # under <清單檔> <目錄>：清單裡有任何路徑等於或在該目錄底下
  awk -v d="$2" '$0 == d || index($0, d "/") == 1 { f = 1; exit } END { exit !f }' "$1"
}
kb_of() { du -sk -- "$1" 2>/dev/null | awk '{print $1}'; }
remove() { # remove <相對路徑>：呼叫端已 cd 進驗證過的目錄
  local kb
  [ ! -L "$1" ] && [ -e "$1" ] || return 1
  kb=$(kb_of "$1"); kb=${kb:-0}
  rm -rf -- "$1" || return 1
  FREED_KB=$((FREED_KB + kb))
}

if [ "$PROC_OK" != 1 ]; then
  log "讀不到 /proc，無法確認有沒有行程在用，這輪不刪任何東西"
  skip "沒有 /proc"
else
  # (a) remote-cargo 快取
  RC="$HOME/.cache/agents-manager/remote-cargo"
  if [ -e "$RC" ] || [ -L "$RC" ]; then
    if safe_dir "$RC" && cd "$RC" 2>/dev/null; then
      while IFS= read -r -d '' d; do
        name="${d#./}"
        if under "$CWDS" "$(phys "$RC/$name")" || under "$FDS" "$(phys "$RC/$name")"; then skip "remote-cargo/${name}（有行程在用）"; continue; fi
        remove "./$name" || skip "remote-cargo/${name}（刪除失敗）"
      done < <(find . -mindepth 1 -maxdepth 1 -type d -mmin +$((REMOTE_DAYS * 1440)) -print0 2>/dev/null)
    else
      skip "remote-cargo（路徑不符預期或含 symlink，整步放棄）"
    fi
  fi

  # (b)(c) 主樹與各 worktree 的 target/
  gc_tree() { # gc_tree <樹的絕對路徑> <main|worktree>
    local tree="$1" kind="$2" prof inc profname
    [ -e "$tree/target" ] || return 0
    if ! safe_dir "$tree" || ! safe_dir "$tree/target"; then skip "${tree}（路徑不符預期或含 symlink）"; return 0; fi
    if [ "$kind" = worktree ] && under "$CWDS" "$(phys "$tree")"; then skip "${tree}（有行程的 cwd 在底下）"; return 0; fi
    if under "$FDS" "$(phys "$tree/target")"; then skip "${tree}（target/ 底下有行程開著檔）"; return 0; fi
    cd "$tree/target" || return 0
    [ "$(pwd -P)" = "$HOME_REAL${tree#"$HOME"}/target" ] || { skip "${tree}（實際位置不符）"; return 0; }
    if [ "$kind" = worktree ]; then
      # 最新的 .fingerprint（含它底下的第一層）都超過門檻才算久沒用；一個 .fingerprint 都沒有就不判斷。
      local have=0 recent=0 fp
      for fp in ./*/.fingerprint; do
        [ -d "$fp" ] && [ ! -L "$fp" ] || continue
        have=1
        if [ -n "$(find "$fp" -maxdepth 1 -mmin -$((TARGET_DAYS * 1440)) -print -quit 2>/dev/null)" ]; then recent=1; fi
      done
      if [ "$have" = 1 ] && [ "$recent" = 0 ]; then
        cd "$tree" || return 0
        remove ./target || skip "${tree}/target（刪除失敗）"
        return 0
      fi
    fi
    for prof in ./*/; do
      profname="${prof%/}"
      [ -d "$profname" ] && [ ! -L "$profname" ] || continue
      inc="$profname/incremental"
      [ -d "$inc" ] && [ ! -L "$inc" ] || continue
      while IFS= read -r -d '' x; do
        remove "$x" || skip "${tree}/target/${x#./}（刪除失敗）"
      done < <(find "$inc" -mindepth 1 -maxdepth 1 -type d -mmin +$((INC_DAYS * 1440)) -print0 2>/dev/null)
    done
  }

  if safe_dir "$AGM_REPO"; then
    gc_tree "${AGM_REPO%/}" main
    WT="${AGM_REPO%/}/.claude/worktrees"
    if [ -e "$WT" ] || [ -L "$WT" ]; then
      if safe_dir "$WT"; then
        for w in "$WT"/*; do
          [ -d "$w" ] && [ ! -L "$w" ] || continue
          gc_tree "$w" worktree
        done
      else
        skip "${WT}（路徑不符預期或含 symlink）"
      fi
    fi
  else
    skip "${AGM_REPO}（路徑不符預期，整步放棄）"
  fi
  cd / || true
fi

freed_mb=$((FREED_KB / 1024))
log "釋放 ${freed_mb} MB${SKIPPED:+；跳過：$SKIPPED}"

# (d) 磁碟水位
cd / || true
line=$("$DF" -Pk "$HOME" 2>/dev/null | awk 'NR == 2')
if [ -n "$line" ]; then
  # shellcheck disable=SC2086
  set -- $line
  avail_kb="$4"; pct="${5%\%}"
  case "$avail_kb$pct" in *[!0-9]*|'') log "df 輸出看不懂：$line"; exit 0 ;; esac
  free_gb=$((avail_kb / 1048576))
  if [ "$pct" -ge "$ALERT_PCT" ] || [ "$free_gb" -lt "$ALERT_FREE_GB" ]; then
    detail="${HOME} 所在磁碟使用率 ${pct}%、剩 ${free_gb} GB（門檻 ${ALERT_PCT}%／${ALERT_FREE_GB} GB）；這輪釋放 ${freed_mb} MB。到 agm-host 查 ~/.cache/agents-manager/remote-cargo 與各 worktree 的 target/"
    log "ALERT disk_low：$detail"
    if [ -x "$AGM" ] || command -v "$AGM" >/dev/null 2>&1; then
      "$AGM" --compact ops-alert --source disk-gc --reason disk_low --detail "$detail" >> "$LOG" 2>&1 || log "推 ops-alert 失敗，只留在這份 log"
    else
      log "找不到 agm（${AGM}），只留在這份 log"
    fi
  fi
fi
exit 0
