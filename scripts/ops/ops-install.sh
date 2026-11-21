#!/bin/bash
set -u

REPO=""; REF=""; DIR=""; PLATFORM=""; DRY=0; FORCE=0
usage() { echo "用法：ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run] [--force]" >&2; }
require_value() {
  if [ "$#" -lt 2 ] || [ -z "${2:-}" ]; then usage; exit 2; fi
}
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) require_value "$@"; REPO="$2"; shift 2 ;;
    --ref) require_value "$@"; REF="$2"; shift 2 ;;
    --dir) require_value "$@"; DIR="$2"; shift 2 ;;
    --platform) require_value "$@"; PLATFORM="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --force) FORCE=1; shift ;;
    *) usage; exit 2 ;;
  esac
done
if [ -z "$REPO" ] || [ -z "$REF" ] || [ -z "$DIR" ]; then
  usage
  exit 2
fi
if [ -z "$PLATFORM" ]; then
  case "$(uname -s)" in Darwin) PLATFORM=darwin ;; *) PLATFORM=linux ;; esac
fi
GIT="${GIT_BIN:-git}"
COMMIT=$("$GIT" -C "$REPO" rev-parse --verify --quiet "${REF}^{commit}") || { echo "找不到 ${REF}" >&2; exit 2; }
MANIFEST=$("$GIT" -C "$REPO" show "${COMMIT}:scripts/ops/install-manifest.tsv" 2>/dev/null) || { echo "${REF} 沒有 scripts/ops/install-manifest.tsv" >&2; exit 2; }

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ops-install.XXXXXX")
LOCK="$DIR/ops-install.lock"; LOCK_GUARD="$DIR/ops-install.lock.guard"; HAVE_LOCK=0
cleanup() { rm -rf "$TMP"; [ "$HAVE_LOCK" = 1 ] && rm -rf "$LOCK"; return 0; }
trap cleanup EXIT
if [ "$DRY" = 0 ]; then
  mkdir -p "$DIR" 2>/dev/null
  acquire_guard() {
    [ ! -L "$LOCK_GUARD" ] || return 2
    exec 9>>"$LOCK_GUARD" || return 2
    python3 -c '
import errno, fcntl, sys
try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError as e:
    sys.exit(1 if e.errno in (errno.EACCES, errno.EAGAIN) else 2)
' >/dev/null 2>&1
  }
  _guard_rc=0
  acquire_guard || _guard_rc=$?
  if [ "$_guard_rc" -eq 1 ]; then
    echo "busy 另一個 ops-install 正在建立或回收鎖，這次不動任何檔" >&2; exit 3
  elif [ "$_guard_rc" -ne 0 ]; then
    echo "busy 無法建立或取得安裝鎖（${LOCK_GUARD}），這次不動任何檔" >&2; exit 3
  fi
  _tries=0
  until mkdir "$LOCK" 2>/dev/null; do
    _holder=$(cat "$LOCK/pid" 2>/dev/null)
    case "$_holder" in ''|*[!0-9]*) _holder="" ;; esac
    if [ -n "$_holder" ] && kill -0 "$_holder" 2>/dev/null; then
      echo "busy 另一個 ops-install（pid ${_holder}）正在裝，這次不動任何檔" >&2; exit 3
    fi
    _tries=$((_tries + 1))
    [ "$_tries" -le 3 ] || { echo "busy 拿不到鎖 ${LOCK}" >&2; exit 3; }
    [ -z "$_holder" ] && sleep 1
    rm -rf "$LOCK" 2>/dev/null
  done
  echo $$ > "$LOCK/pid"; HAVE_LOCK=1
fi
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP="$DIR/ops-install-backups/$STAMP"
_n=1; while [ -e "$BACKUP" ]; do _n=$((_n + 1)); BACKUP="$DIR/ops-install-backups/$STAMP-$_n"; done
changes=0; failed=0

dest_changed() {
  if [ "$fresh" = 1 ]; then [ -e "$dest" ] || [ -L "$dest" ]; else [ -L "$dest" ] || ! cmp -s "$snapshot" "$dest"; fi
}

selfcheck() {
  case "$1" in
    *.sh)
      _shebang=""
      IFS= read -r _shebang < "$2" 2>/dev/null || true
      case "$_shebang" in
        '#!/bin/zsh'*|'#!/usr/bin/env zsh'*|'#! /bin/zsh'*|'#! /usr/bin/env zsh'*|'#!'*'/zsh'*)
          if command -v zsh >/dev/null 2>&1; then
            zsh -n "$2" 2>&1
          else
            echo "SKIPPED_NO_ZSH"
            return 2
          fi
          ;;
        *)
          bash -n "$2" 2>&1
          ;;
      esac
      ;;
    *.py) python3 -B -c 'import sys; compile(open(sys.argv[1]).read(), sys.argv[1], "exec")' "$2" 2>&1 ;;
    *.ts) if command -v bun >/dev/null 2>&1; then bun build --target=bun --outfile=/dev/null "$2" 2>&1 >/dev/null; fi ;;
    *) return 0 ;;
  esac
}

known_version() {
  _have=$("$GIT" hash-object "$2" 2>/dev/null) || return 1
  "$GIT" -C "$REPO" log --format=%H "$COMMIT" -- "$1" 2>/dev/null | while IFS= read -r _c; do
    [ "$("$GIT" -C "$REPO" rev-parse --verify --quiet "${_c}:$1" 2>/dev/null)" = "$_have" ] && { echo found; break; }
  done | grep -q found
}

has_symlink_parent() {
  _rel="$1"
  _parent=${_rel%/*}
  [ "$_parent" = "$_rel" ] && return 1
  _path="$DIR"
  _parts="$_parent/"
  while [ -n "$_parts" ]; do
    _part=${_parts%%/*}
    _parts=${_parts#*/}
    [ -n "$_part" ] || continue
    _path="$_path/$_part"
    [ -L "$_path" ] && return 0
  done
  return 1
}

parse_flags() {
  plat=""; shared=0; shift 2
  for _t in "$@"; do case "$_t" in shared) shared=1 ;; *) plat="$_t" ;; esac; done
}

if [ "$DRY" = 0 ]; then
  : > "$TMP/clean"
  printf '%s\n' "$MANIFEST" | while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue ;; esac
    # shellcheck disable=SC2086
    set -- $line
    [ $# -ge 2 ] || continue
    tgt="$2"; parse_flags "$@"
    [ -z "$plat" ] || [ "$plat" = "$PLATFORM" ] || continue
    case "$tgt" in systemd/*|LaunchAgents/*|/*|..|../*|*/..|*/../*) continue ;; esac
    has_symlink_parent "$tgt" && continue
    for f in "$DIR/$tgt".new.*; do
      [ -f "$f" ] || continue
      pid="${f##*.new.}"
      case "$pid" in ''|*[!0-9]*) continue ;; esac
      kill -0 "$pid" 2>/dev/null && continue
      rm -f "$f" && echo "$f" >> "$TMP/clean"
    done
  done
  _cleaned=$(wc -l < "$TMP/clean" | tr -d ' ')
  [ "${_cleaned:-0}" -eq 0 ] || echo "cleaned ${_cleaned}（被中斷的上一次留下的暫存檔：$(tr '\n' ' ' < "$TMP/clean" | sed "s#$DIR/##g")）"
fi

run_pass() {
PASS="$1"
printf '%s\n' "$MANIFEST" | while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  # shellcheck disable=SC2086
  set -- $line
  [ $# -ge 2 ] || continue
  src="$1"; tgt="$2"; parse_flags "$@"
  [ "$shared" = "$PASS" ] || continue
  [ -z "$plat" ] || [ "$plat" = "$PLATFORM" ] || continue
  case "$tgt" in
    systemd/*|LaunchAgents/*) echo "skipped ${tgt}（排程 unit 要 daemon-reload／launchctl，不自動換）"; continue ;;
  esac
  case "$tgt" in
    /*|..|../*|*/..|*/../*) echo "failed ${tgt}（安裝位置跑出 --dir，不寫）"; echo F >> "$TMP/results"; continue ;;
  esac
  if has_symlink_parent "$tgt"; then
    echo "failed ${tgt}（安裝位置父目錄是 symlink，不寫）"; echo F >> "$TMP/results"; continue
  fi
  dest="$DIR/$tgt"
  if [ -L "$dest" ]; then echo "skipped ${tgt}（是 symlink，不換；連結指到哪就是哪版）"; continue; fi
  fresh=0
  if [ ! -f "$dest" ]; then
    if [ "$shared" = 1 ]; then fresh=1; else echo "not-installed ${tgt}（新檔，第一次要手動裝）"; continue; fi
  fi
  new="$TMP/new"
  if ! "$GIT" -C "$REPO" show "${COMMIT}:${src}" > "$new" 2>/dev/null; then
    echo "failed ${tgt}（${REF} 裡讀不到 ${src}）"; echo F >> "$TMP/results"; continue
  fi
  snapshot="$TMP/original"
  if [ "$fresh" = 0 ] && ! cp "$dest" "$snapshot"; then
    echo "failed ${tgt}（讀不了安裝端的舊檔，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  if [ "$fresh" = 0 ] && cmp -s "$new" "$snapshot"; then echo "unchanged $tgt"; continue; fi
  if [ "$fresh" = 0 ] && [ "$FORCE" != 1 ] && ! known_version "$src" "$snapshot"; then
    echo "drifted ${tgt}（安裝端的檔不是 repo 任何一版，有人手改過；沒動它。要換就先看差在哪，或加 --force，舊檔照樣會備份）"
    echo D >> "$TMP/results"; continue
  fi
  echo C >> "$TMP/results"
  if [ "$DRY" = 1 ]; then echo "would-install $tgt"; continue; fi
  mode=$(stat -c %a "$dest" 2>/dev/null || stat -f %Lp "$dest" 2>/dev/null || echo 644)
  [ "$fresh" = 1 ] && mkdir -p "${dest%/*}"
  stage="$dest.new.$$"
  if ! { cp "$new" "$stage" && chmod "$mode" "$stage"; }; then
    rm -f "$stage"; echo "failed ${tgt}（寫不進去，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  _sc_file="$TMP/selfcheck.${tgt##*.}"
  if ! cp "$stage" "$_sc_file"; then
    rm -f "$stage"; echo "failed ${tgt}（暫存不了自檢用的檔，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  _sc_rc=0
  err=$(selfcheck "$tgt" "$_sc_file") || _sc_rc=$?
  rm -f "$_sc_file"
  if [ "$_sc_rc" -eq 2 ] && [ "$err" = "SKIPPED_NO_ZSH" ]; then
    rm -f "$stage"
    sed -e '$d' "$TMP/results" > "$TMP/results.tmp" 2>/dev/null && mv "$TMP/results.tmp" "$TMP/results"
    echo "skipped ${tgt}（未安裝 zsh，跳過自檢與安裝，舊版原封不動）"
    continue
  elif [ "$_sc_rc" -ne 0 ]; then
    rm -f "$stage"
    echo "failed ${tgt}（自檢沒過，沒換，舊版原封不動）：$(printf '%s' "$err" | head -2 | tr '\n' ' ')"
    echo F >> "$TMP/results"; continue
  fi
  if has_symlink_parent "$tgt" || dest_changed; then
    rm -f "$stage"
    echo "drifted ${tgt}（安裝期間目的地改變，保留現況）"
    echo D >> "$TMP/results"; continue
  fi
  if [ "$fresh" = 0 ]; then mkdir -p "$BACKUP/$(dirname "$tgt")"; fi
  if [ "$fresh" = 0 ] && ! cp -p "$dest" "$BACKUP/$tgt"; then rm -f "$stage"; echo "failed ${tgt}（備份不了，沒動它）"; echo F >> "$TMP/results"; continue; fi
  if has_symlink_parent "$tgt" || dest_changed; then
    rm -f "$stage"
    echo "drifted ${tgt}（備份期間目的地改變，保留現況）"
    echo D >> "$TMP/results"; continue
  fi
  if ! mv -f "$stage" "$dest"; then
    rm -f "$stage"; echo "failed ${tgt}（換不進去，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  if [ "$fresh" = 1 ]; then echo "installed ${tgt}（新檔：被 kick source 的共用檔，安裝端沒有就先裝）"
  else echo "installed ${tgt}（舊檔備份在 ${BACKUP}/${tgt}）"; fi
done
}
PASS=0
{ run_pass 1; run_pass 0; } > "$TMP/lines"

cat "$TMP/lines"
changes=$(grep -c '^C$' "$TMP/results" 2>/dev/null || true)
failed=$(grep -c '^F$' "$TMP/results" 2>/dev/null || true)
drifted=$(grep -c '^D$' "$TMP/results" 2>/dev/null || true)
echo "changes=${changes:-0} failed=${failed:-0} drifted=${drifted:-0}"
if [ "$DRY" = 0 ]; then
  if [ "${failed:-0}" -gt 0 ]; then
    printf '%s %s failed=%s\n' "$STAMP" "$COMMIT" "$failed" > "$DIR/ops-install.last-failed"
  else
    rm -f "$DIR/ops-install.last-failed"
    [ "${changes:-0}" -gt 0 ] && printf '%s %s\n' "$STAMP" "$COMMIT" > "$DIR/ops-install.last"
  fi
fi
[ "${failed:-0}" = 0 ]
