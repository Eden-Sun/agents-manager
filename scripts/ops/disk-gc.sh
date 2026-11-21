#!/bin/bash
set -u

DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOG="${DISK_GC_LOG:-$DIR/disk-gc.log}"
AGM_REPO="${AGM_REPO:-$HOME/project/agents-manager}"
AGM="${AGM_BIN:-$DIR/bin/agm}"
DF="${DF_BIN:-df}"
GIT="${DISK_GC_GIT:-git}"
CI_ROOT="${AGM_CI_ROOT:-$HOME/.cache/agents-manager/ci}"
REMOTE_DAYS="${DISK_GC_REMOTE_CARGO_DAYS:-7}"
INC_DAYS="${DISK_GC_INCREMENTAL_DAYS:-2}"
POOL_HOURS="${DISK_GC_POOL_INCREMENTAL_HOURS:-6}"
TARGET_DAYS="${DISK_GC_TARGET_DAYS:-7}"
ALERT_PCT="${DISK_GC_ALERT_PCT:-85}"
ALERT_FREE_GB="${DISK_GC_ALERT_FREE_GB:-20}"
for v in REMOTE_DAYS INC_DAYS TARGET_DAYS POOL_HOURS ALERT_PCT ALERT_FREE_GB; do
  case "${!v}" in ''|*[!0-9]*) echo "disk-gc: ${v} 不是數字（${!v}）" >&2; exit 2 ;; esac
done

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

HOME_REAL=$(cd "$HOME" 2>/dev/null && pwd -P) || { log "拒絕：無法解析 HOME"; exit 1; }
case "$HOME" in /*) ;; *) log "拒絕：HOME 不是絕對路徑"; exit 1 ;; esac

FREED_KB=0
SKIPPED=""
skip() { SKIPPED="${SKIPPED}${SKIPPED:+；}$1"; }

safe_dir() {
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
phys() { echo "$HOME_REAL${1#"$HOME"}"; }
under() {
  awk -v d="$2" '$0 == d || index($0, d "/") == 1 { f = 1; exit } END { exit !f }' "$1"
}
kb_of() { du -sk -- "$1" 2>/dev/null | awk '{print $1}'; }
remove() {
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

  if [ -d "$RC" ] && [ ! -L "$RC" ] && safe_dir "$RC" && cd "$RC" 2>/dev/null; then
    for h in ./*/; do
      h="${h%/}"; [ -d "$h" ] && [ ! -L "$h" ] || continue
      for s in "$h"/shared*; do
        [ -e "$s" ] || [ -L "$s" ] || continue
        if [ -L "$s" ] || ! safe_dir "$RC/${s#./}"; then skip "remote-cargo/${s#./}（路徑不符預期或含 symlink）"; continue; fi
        [ -d "$s" ] || continue
        for pr in "$s"/target/*; do
          [ -d "$pr" ] && [ ! -L "$pr" ] || continue
          inc="$pr/incremental"
          [ -d "$inc" ] && [ ! -L "$inc" ] || continue
          while IFS= read -r -d '' x; do
            name="${x#./}"
            if under "$CWDS" "$(phys "$RC/$name")" || under "$FDS" "$(phys "$RC/$name")"; then skip "remote-cargo/${name}（有行程在用）"; continue; fi
            recent=$(find "$x" -mmin -$((POOL_HOURS * 60)) -print -quit 2>/dev/null); rc=$?
            if [ -n "$recent" ] || [ "$rc" -ne 0 ]; then continue; fi
            remove "$x" || skip "remote-cargo/${name}（刪除失敗）"
          done < <(find "$inc" -mindepth 1 -maxdepth 1 -type d -print0 2>/dev/null)
        done
      done
    done
  fi

  gc_tree() {
    local tree="$1" kind="$2" prof inc profname
    [ -e "$tree/target" ] || return 0
    if ! safe_dir "$tree" || ! safe_dir "$tree/target"; then skip "${tree}（路徑不符預期或含 symlink）"; return 0; fi
    if [ "$kind" = worktree ] && under "$CWDS" "$(phys "$tree")"; then skip "${tree}（有行程的 cwd 在底下）"; return 0; fi
    if under "$FDS" "$(phys "$tree/target")"; then skip "${tree}（target/ 底下有行程開著檔）"; return 0; fi
    cd "$tree/target" || return 0
    [ "$(pwd -P)" = "$HOME_REAL${tree#"$HOME"}/target" ] || { skip "${tree}（實際位置不符）"; return 0; }
    if [ "$kind" = worktree ]; then
      local have=0 recent=0 unreadable=0 fp out rc
      for fp in ./*/.fingerprint; do
        [ -d "$fp" ] && [ ! -L "$fp" ] || continue
        have=1
        out=$(find "$fp" -maxdepth 2 -mmin -$((TARGET_DAYS * 1440)) -print -quit 2>/dev/null); rc=$?
        if [ -n "$out" ]; then recent=1
        elif [ "$rc" -ne 0 ]; then unreadable=1
        fi
      done
      if [ "$unreadable" = 1 ]; then
        skip "${tree}/target（.fingerprint 讀不完，不能證明久沒用，整棵保留）"
      elif [ "$have" = 1 ] && [ "$recent" = 0 ]; then
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
    GT_DONE=""
    if WTS=$("$GIT" -C "$AGM_REPO" worktree list --porcelain 2>/dev/null); then
      while IFS= read -r line; do
        case "$line" in "worktree "*) ;; *) continue ;; esac
        w="${line#worktree }"
        [ "$w" = "${AGM_REPO%/}" ] && continue
        GT_DONE="${GT_DONE}${w}
"
        gc_tree "$w" worktree
      done <<EOF_WTS
$WTS
EOF_WTS
    else
      skip "git worktree list 失敗，只掃 .claude/worktrees"
    fi
    WT="${AGM_REPO%/}/.claude/worktrees"
    if [ -e "$WT" ] || [ -L "$WT" ]; then
      if safe_dir "$WT"; then
        for w in "$WT"/*; do
          [ -d "$w" ] && [ ! -L "$w" ] || continue
          printf '%s\n' "$GT_DONE" | grep -qxF "$w" && continue
          gc_tree "$w" worktree
        done
      else
        skip "${WT}（路徑不符預期或含 symlink）"
      fi
    fi
  else
    skip "${AGM_REPO}（路徑不符預期，整步放棄）"
  fi
  CI_REPO="${CI_ROOT}/repo"
  if [ -e "$CI_REPO" ] || [ -L "$CI_REPO" ]; then
    gc_tree "$CI_REPO" main
  fi
  cd / || true
fi

freed_mb=$((FREED_KB / 1024))
log "釋放 ${freed_mb} MB${SKIPPED:+；跳過：$SKIPPED}"

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
