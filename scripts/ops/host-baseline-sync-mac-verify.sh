#!/bin/sh
# issue #719 階段 3a：主機一致性補齊（apply）與還原（restore）腳本的原生驗證（macOS 的 /bin/sh、BSD 工具；Linux 也能跑）。
#
# 用法：sh scripts/ops/host-baseline-sync-mac-verify.sh
#
# 下面三段 heredoc 是 daemon 產生、送到主機上跑的同一份腳本（`crates/am-base/src/host_baseline/apply.rs`；
# `cargo test -p am-base the_mac_verify_script_embeds_the_current_scripts` 守著它跟目前產生的一致）。
# 全部在 mktemp -d 的假 HOME 裡跑、env -i 清掉環境，結束（含中斷）就整個刪掉；不碰真的 HOME。
# 驗：列舉唯讀與壞備份／symlink 略過、只補不蓋、寫前備份與 manifest、權限、還原後逐 byte 相同、不可信 manifest 整個拒絕、10 份輪替、輸出不帶路徑。
set -u
WORK=$(mktemp -d "${TMPDIR:-/tmp}/am-bl-verify.XXXXXX") || exit 1
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM HUP

cat > "$WORK/apply.sh" <<'AM_APPLY_SH'
umask 077
ba_root="$HOME/.config/agents-manager/baseline-backup"
# 只認這幾個目標（宣告檔的 codex config.toml、claude settings.json）；不收絕對路徑、`..`、空白。
ba_token_ok() {
  case "$1" in ''|/*|*..*|*//*|*' '*) return 1 ;; esac
  case "$1" in
    codex/config.toml|claude-default/settings.json) return 0 ;;
    claude-cc*/settings.json)
      ba_t=${1#claude-cc}; ba_t=${ba_t%/settings.json}
      case "$ba_t" in ''|*[!0-9]*) return 1 ;; esac
      return 0 ;;
  esac
  return 1
}
ba_path() {
  ba_token_ok "$1" || return 1
  case "$1" in
    codex/config.toml) printf '%s/config.toml' "${CODEX_HOME:-$HOME/.codex}" ;;
    claude-default/settings.json) printf '%s/.claude/settings.json' "$HOME" ;;
    *) ba_t=${1%/settings.json}; printf '%s/.%s/settings.json' "$HOME" "$ba_t" ;;
  esac
}
# 權限（八進位三位）；讀不懂＝`-`（不 chmod）。
ba_mode() {
  ba_p=$(ls -ld "$1" 2>/dev/null | cut -c2-10)
  case "$ba_p" in [r-][w-][xsS-][r-][w-][xsS-][r-][w-][xtT-]) ;; *) printf -; return ;; esac
  ba_o=""
  for ba_i in 1 4 7; do
    ba_d=0
    [ "$(printf %s "$ba_p" | cut -c$ba_i)" = r ] && ba_d=$((ba_d + 4))
    [ "$(printf %s "$ba_p" | cut -c$((ba_i + 1)))" = w ] && ba_d=$((ba_d + 2))
    case "$(printf %s "$ba_p" | cut -c$((ba_i + 2)))" in x|s|t) ba_d=$((ba_d + 1)) ;; esac
    ba_o="$ba_o$ba_d"
  done
  printf %s "$ba_o"
}
ba_sync() { sync "$1" 2>/dev/null || sync; }
# 備份根目錄與它上面兩層不能是 symlink。
ba_root_ok() {
  for ba_q in "$HOME/.config" "$HOME/.config/agents-manager" "$ba_root"; do
    [ -L "$ba_q" ] && return 1
  done
  return 0
}
ba_id='20261024T000000Z-00000000-0000-4000-8000-000000000000'
ba_rev='macverify'

printf 'AM_BA begin\n'
ba_end() { printf 'AM_BA end\n'; exit 0; }
case "$HOME" in /*) ;; *) printf 'AM_BA error no_home\n'; ba_end ;; esac
ba_awk='
function keyname(l,   i, p) { i = index(l, "="); if (i == 0) return ""; p = substr(l, 1, i - 1); sub(/[[:blank:]]+$/, "", p); return p }
BEGIN { t = ENVIRON["AM_T"]; k = ENVIRON["AM_K"]; v = ENVIRON["AM_V"]; root = 1; intab = 0; found = 0; tabline = 0; first = 0 }
{
  lines[NR] = $0
  l = $0
  sub(/\r$/, "", l)
  sub(/^[[:blank:]]+/, "", l)
  if (l == "" || l ~ /^#/) next
  if (l ~ /^\[/) {
    if (!first) first = NR
    root = 0
    h = l
    sub(/\][[:blank:]]*(#.*)?$/, "]", h)
    gsub(/[[:blank:]]/, "", h)
    intab = (t != "" && h == "[" t "]")
    if (intab && !tabline) tabline = NR
    next
  }
  n = keyname(l)
  gsub(/[[:blank:]]/, "", n)
  if (t == "") { if (root && n == k) found = 1 }
  else if ((root && (n == t "." k || n == t)) || (intab && n == k)) found = 1
}
END {
  if (found) exit 3
  if (t != "" && tabline) at = tabline + 1
  else at = first ? first : NR + 1
  line = (t != "" && !tabline) ? t "." k " = " v : k " = " v
  for (i = 1; i <= NR; i++) {
    if (i == at) print line
    print lines[i]
  }
  if (at > NR) print line
}
'
ba_jq=$(command -v jq 2>/dev/null || true)
case "$ba_jq" in /*) [ -x "$ba_jq" ] || ba_jq="" ;; *) ba_jq="" ;; esac
ba_dir=""
ba_seen="|"
ba_n=0
ba_stop=0
# 第一次真的要寫才建備份目錄；同一個檔只備份一次。manifest：每行 `file <編號> <原本在不在> <權限> <目標>`。
ba_backup() {
  case "$ba_seen" in *"|$1|"*) return 0 ;; esac
  if [ -z "$ba_dir" ]; then
    ba_root_ok || return 1
    mkdir -p "$ba_root" 2>/dev/null && chmod 700 "$ba_root" 2>/dev/null || return 1
    ba_new="$ba_root/$ba_id"
    mkdir "$ba_new" 2>/dev/null && mkdir "$ba_new/files" 2>/dev/null || return 1
    chmod 700 "$ba_new" "$ba_new/files" 2>/dev/null || return 1
    { printf 'am-baseline-backup 1\n'; printf 'id %s\n' "$ba_id"; printf 'plan_rev %s\n' "$ba_rev"; } > "$ba_new/manifest" 2>/dev/null || return 1
    chmod 600 "$ba_new/manifest" 2>/dev/null || return 1
    ba_dir=$ba_new
  fi
  ba_n=$((ba_n + 1))
  cp "$2" "$ba_dir/files/$ba_n" 2>/dev/null && chmod 600 "$ba_dir/files/$ba_n" 2>/dev/null || return 1
  printf 'file %s 1 %s %s\n' "$ba_n" "$(ba_mode "$2")" "$1" >> "$ba_dir/manifest" 2>/dev/null || return 1
  ba_seen="$ba_seen$1|"
}
ba_key() { printf 'AM_BA key %s %s\n' "$@"; }
# ba_one <編號> <目標> toml <表> <鍵> <值> ｜ ba_one <編號> <目標> json <點號路徑> <JSON 值>
ba_one() {
  ba_i=$1
  ba_tok=$2
  if [ "$ba_stop" = 1 ]; then ba_key "$ba_i" "skipped aborted"; return; fi
  ba_f=$(ba_path "$ba_tok") || { ba_key "$ba_i" "skipped bad_target"; return; }
  ba_d=$(dirname "$ba_f")
  if [ -L "$ba_f" ] || [ -L "$ba_d" ]; then ba_key "$ba_i" "skipped symlink"; return; fi
  [ -f "$ba_f" ] || { ba_key "$ba_i" "skipped file_missing"; return; }
  if [ "$3" = json ] && [ -z "$ba_jq" ]; then ba_key "$ba_i" "manual no_jq"; return; fi
  ba_tmp=$(mktemp "$ba_d/.am-baseline.XXXXXX" 2>/dev/null) || { ba_key "$ba_i" "skipped write_failed"; return; }
  if [ "$3" = toml ]; then
    AM_T=$4 AM_K=$5 AM_V=$6 /usr/bin/awk "$ba_awk" "$ba_f" > "$ba_tmp" 2>/dev/null
    ba_rc=$?
  else
    "$ba_jq" -e --arg p "$4" 'def bl_h($ks): if ($ks | length) == 0 then true elif type == "object" and has($ks[0]) then (.[$ks[0]] | bl_h($ks[1:])) else false end; bl_h($p | split("."))' "$ba_f" >/dev/null 2>&1
    case $? in
      0) ba_rc=3 ;;
      1) "$ba_jq" --arg p "$4" --argjson v "$5" 'setpath($p | split("."); $v)' "$ba_f" > "$ba_tmp" 2>/dev/null && ba_rc=0 || ba_rc=4 ;;
      *) ba_rc=5 ;;
    esac
  fi
  case $ba_rc in
    0) ;;
    3) rm -f "$ba_tmp"; ba_key "$ba_i" present; return ;;
    5) rm -f "$ba_tmp"; ba_key "$ba_i" "skipped invalid_json"; return ;;
    *) rm -f "$ba_tmp"; ba_key "$ba_i" "skipped conflict"; return ;;
  esac
  if ! ba_backup "$ba_tok" "$ba_f"; then
    rm -f "$ba_tmp"
    ba_stop=1
    printf 'AM_BA error backup_failed\n'
    ba_key "$ba_i" "skipped backup_failed"
    return
  fi
  ba_m=$(ba_mode "$ba_f")
  ba_sync "$ba_tmp"
  [ "$ba_m" = - ] || chmod "$ba_m" "$ba_tmp" 2>/dev/null
  if mv -f "$ba_tmp" "$ba_f" 2>/dev/null; then ba_key "$ba_i" applied; else rm -f "$ba_tmp"; ba_key "$ba_i" "skipped write_failed"; fi
}
ba_one 0 'codex/config.toml' toml 'features' 'hooks' 'true'

if [ -n "$ba_dir" ]; then
  ba_c=0
  for ba_x in "$ba_root"/[0-9]*T[0-9]*Z-*; do [ -d "$ba_x" ] && [ ! -L "$ba_x" ] && ba_c=$((ba_c + 1)); done
  for ba_x in "$ba_root"/[0-9]*T[0-9]*Z-*; do
    [ "$ba_c" -gt 10 ] || break
    [ -d "$ba_x" ] && [ ! -L "$ba_x" ] || continue
    [ "$ba_x" = "$ba_dir" ] && continue
    rm -rf "$ba_x" && ba_c=$((ba_c - 1))
  done
  printf 'AM_BA backup\n'
fi
ba_end
AM_APPLY_SH

cat > "$WORK/restore.sh" <<'AM_RESTORE_SH'
umask 077
ba_root="$HOME/.config/agents-manager/baseline-backup"
# 只認這幾個目標（宣告檔的 codex config.toml、claude settings.json）；不收絕對路徑、`..`、空白。
ba_token_ok() {
  case "$1" in ''|/*|*..*|*//*|*' '*) return 1 ;; esac
  case "$1" in
    codex/config.toml|claude-default/settings.json) return 0 ;;
    claude-cc*/settings.json)
      ba_t=${1#claude-cc}; ba_t=${ba_t%/settings.json}
      case "$ba_t" in ''|*[!0-9]*) return 1 ;; esac
      return 0 ;;
  esac
  return 1
}
ba_path() {
  ba_token_ok "$1" || return 1
  case "$1" in
    codex/config.toml) printf '%s/config.toml' "${CODEX_HOME:-$HOME/.codex}" ;;
    claude-default/settings.json) printf '%s/.claude/settings.json' "$HOME" ;;
    *) ba_t=${1%/settings.json}; printf '%s/.%s/settings.json' "$HOME" "$ba_t" ;;
  esac
}
# 權限（八進位三位）；讀不懂＝`-`（不 chmod）。
ba_mode() {
  ba_p=$(ls -ld "$1" 2>/dev/null | cut -c2-10)
  case "$ba_p" in [r-][w-][xsS-][r-][w-][xsS-][r-][w-][xtT-]) ;; *) printf -; return ;; esac
  ba_o=""
  for ba_i in 1 4 7; do
    ba_d=0
    [ "$(printf %s "$ba_p" | cut -c$ba_i)" = r ] && ba_d=$((ba_d + 4))
    [ "$(printf %s "$ba_p" | cut -c$((ba_i + 1)))" = w ] && ba_d=$((ba_d + 2))
    case "$(printf %s "$ba_p" | cut -c$((ba_i + 2)))" in x|s|t) ba_d=$((ba_d + 1)) ;; esac
    ba_o="$ba_o$ba_d"
  done
  printf %s "$ba_o"
}
ba_sync() { sync "$1" 2>/dev/null || sync; }
# 備份根目錄與它上面兩層不能是 symlink。
ba_root_ok() {
  for ba_q in "$HOME/.config" "$HOME/.config/agents-manager" "$ba_root"; do
    [ -L "$ba_q" ] && return 1
  done
  return 0
}
br_id='20261024T000000Z-00000000-0000-4000-8000-000000000000'

printf 'AM_BR begin\n'
br_end() { printf 'AM_BR end\n'; exit 0; }
br_fail() { printf 'AM_BR error %s\n' "$1"; br_end; }
case "$HOME" in /*) ;; *) br_fail no_home ;; esac
br_dir="$ba_root/$br_id"
ba_root_ok || br_fail symlink
for br_q in "$br_dir" "$br_dir/files" "$br_dir/manifest"; do [ -L "$br_q" ] && br_fail symlink; done
[ -d "$br_dir" ] || br_fail not_found
[ -f "$br_dir/manifest" ] || br_fail manifest_invalid
set -f
# 第一趟：整份 manifest 驗過才動手。任一行不對＝整個拒絕，什麼都不寫。
br_hdr=0
br_count=0
br_seen="|"
while IFS= read -r br_l || [ -n "$br_l" ]; do
  if [ "$br_hdr" = 0 ]; then
    [ "$br_l" = "am-baseline-backup 1" ] || br_fail manifest_invalid
    br_hdr=1
    continue
  fi
  set -- $br_l
  case "$1" in
    id) [ $# -eq 2 ] && [ "$2" = "$br_id" ] || br_fail manifest_invalid ;;
    plan_rev) [ $# -eq 2 ] || br_fail manifest_invalid ;;
    file)
      [ $# -eq 5 ] || br_fail manifest_invalid
      case "$2" in ''|*[!0-9]*) br_fail manifest_invalid ;; esac
      case "$3" in 0|1) ;; *) br_fail manifest_invalid ;; esac
      case "$4" in -|[0-7][0-7][0-7]) ;; *) br_fail manifest_invalid ;; esac
      ba_token_ok "$5" || br_fail manifest_invalid
      case "$br_seen" in *"|$5|"*) br_fail manifest_invalid ;; esac
      br_seen="$br_seen$5|"
      if [ "$3" = 1 ]; then
        [ -L "$br_dir/files/$2" ] && br_fail symlink
        [ -f "$br_dir/files/$2" ] || br_fail manifest_invalid
      fi
      br_f=$(ba_path "$5") || br_fail manifest_invalid
      br_d=$(dirname "$br_f")
      if [ -L "$br_f" ] || [ -L "$br_d" ]; then br_fail symlink; fi
      [ -d "$br_d" ] || br_fail target_dir_missing
      if [ -e "$br_f" ] && [ ! -f "$br_f" ]; then br_fail not_a_file; fi
      br_count=$((br_count + 1))
      ;;
    *) br_fail manifest_invalid ;;
  esac
done < "$br_dir/manifest"
[ "$br_hdr" = 1 ] && [ "$br_count" -gt 0 ] || br_fail manifest_invalid
# 第二趟：照驗過的內容還原。原本在的檔：同目錄暫存檔＋sync＋原權限＋mv；原本不在的：刪掉後來建的。
br_hdr=0
while IFS= read -r br_l || [ -n "$br_l" ]; do
  if [ "$br_hdr" = 0 ]; then br_hdr=1; continue; fi
  set -- $br_l
  [ "$1" = file ] || continue
  br_f=$(ba_path "$5")
  if [ "$3" = 0 ]; then
    if [ -f "$br_f" ] && rm -f "$br_f" 2>/dev/null; then printf 'AM_BR file %s deleted\n' "$5"; else printf 'AM_BR file %s absent\n' "$5"; fi
    continue
  fi
  br_d=$(dirname "$br_f")
  br_tmp=$(mktemp "$br_d/.am-baseline.XXXXXX" 2>/dev/null) || { printf 'AM_BR file %s failed\n' "$5"; continue; }
  if cat "$br_dir/files/$2" > "$br_tmp" 2>/dev/null; then
    ba_sync "$br_tmp"
    [ "$4" = - ] || chmod "$4" "$br_tmp" 2>/dev/null
    if mv -f "$br_tmp" "$br_f" 2>/dev/null; then printf 'AM_BR file %s restored\n' "$5"; continue; fi
  fi
  rm -f "$br_tmp"
  printf 'AM_BR file %s failed\n' "$5"
done < "$br_dir/manifest"
br_end
AM_RESTORE_SH

cat > "$WORK/list.sh" <<'AM_LIST_SH'
umask 077
ba_root="$HOME/.config/agents-manager/baseline-backup"
# 只認這幾個目標（宣告檔的 codex config.toml、claude settings.json）；不收絕對路徑、`..`、空白。
ba_token_ok() {
  case "$1" in ''|/*|*..*|*//*|*' '*) return 1 ;; esac
  case "$1" in
    codex/config.toml|claude-default/settings.json) return 0 ;;
    claude-cc*/settings.json)
      ba_t=${1#claude-cc}; ba_t=${ba_t%/settings.json}
      case "$ba_t" in ''|*[!0-9]*) return 1 ;; esac
      return 0 ;;
  esac
  return 1
}
ba_path() {
  ba_token_ok "$1" || return 1
  case "$1" in
    codex/config.toml) printf '%s/config.toml' "${CODEX_HOME:-$HOME/.codex}" ;;
    claude-default/settings.json) printf '%s/.claude/settings.json' "$HOME" ;;
    *) ba_t=${1%/settings.json}; printf '%s/.%s/settings.json' "$HOME" "$ba_t" ;;
  esac
}
# 權限（八進位三位）；讀不懂＝`-`（不 chmod）。
ba_mode() {
  ba_p=$(ls -ld "$1" 2>/dev/null | cut -c2-10)
  case "$ba_p" in [r-][w-][xsS-][r-][w-][xsS-][r-][w-][xtT-]) ;; *) printf -; return ;; esac
  ba_o=""
  for ba_i in 1 4 7; do
    ba_d=0
    [ "$(printf %s "$ba_p" | cut -c$ba_i)" = r ] && ba_d=$((ba_d + 4))
    [ "$(printf %s "$ba_p" | cut -c$((ba_i + 1)))" = w ] && ba_d=$((ba_d + 2))
    case "$(printf %s "$ba_p" | cut -c$((ba_i + 2)))" in x|s|t) ba_d=$((ba_d + 1)) ;; esac
    ba_o="$ba_o$ba_d"
  done
  printf %s "$ba_o"
}
ba_sync() { sync "$1" 2>/dev/null || sync; }
# 備份根目錄與它上面兩層不能是 symlink。
ba_root_ok() {
  for ba_q in "$HOME/.config" "$HOME/.config/agents-manager" "$ba_root"; do
    [ -L "$ba_q" ] && return 1
  done
  return 0
}

printf 'AM_BK begin\n'
bk_end() { printf 'AM_BK skipped %s\n' "$bk_skipped"; printf 'AM_BK end\n'; exit 0; }
bk_skipped=0
bk_id_ok() {
  case "$1" in ????????T??????Z-????????-????-????-????-????????????) ;; *) return 1 ;; esac
  bk_stamp=${1%%-*}
  case "$bk_stamp" in *[!0123456789TZ]*) return 1 ;; esac
  bk_uuid=${1#????????T??????Z-}
  case "$bk_uuid" in *[!0123456789abcdef-]*) return 1 ;; esac
  return 0
}
case "$HOME" in /*) ;; *) bk_skipped=1; bk_end ;; esac
if ! ba_root_ok; then bk_skipped=1; bk_end; fi
if [ -L "$ba_root" ]; then bk_skipped=1; bk_end; fi
if [ ! -e "$ba_root" ]; then bk_end; fi
if [ ! -d "$ba_root" ]; then bk_skipped=1; bk_end; fi
for bk_dir in "$ba_root"/*; do
  [ -e "$bk_dir" ] || [ -L "$bk_dir" ] || continue
  bk_id=${bk_dir##*/}
  bk_id_ok "$bk_id" || continue
  bk_bad=0
  if [ -L "$bk_dir" ] || [ ! -d "$bk_dir" ]; then bk_bad=1; fi
  bk_files_dir="$bk_dir/files"
  bk_manifest="$bk_dir/manifest"
  if [ "$bk_bad" = 0 ] && { [ -L "$bk_files_dir" ] || [ ! -d "$bk_files_dir" ] || [ -L "$bk_manifest" ] || [ ! -f "$bk_manifest" ]; }; then bk_bad=1; fi
  bk_header=0
  bk_id_seen=0
  bk_plan_seen=0
  bk_count=0
  bk_seen="|"
  bk_file_ids="|"
  bk_meta=""
  if [ "$bk_bad" = 0 ]; then
    set -f
    while IFS= read -r bk_line || [ -n "$bk_line" ]; do
      if [ "$bk_header" = 0 ]; then
        [ "$bk_line" = "am-baseline-backup 1" ] || { bk_bad=1; break; }
        bk_header=1
        continue
      fi
      set -- $bk_line
      [ $# -gt 0 ] || { bk_bad=1; break; }
      case "$1" in
        id)
          [ $# -eq 2 ] && [ "$bk_id_seen" = 0 ] && [ "$2" = "$bk_id" ] || { bk_bad=1; break; }
          bk_id_seen=1
          ;;
        plan_rev)
          [ $# -eq 2 ] && [ "$bk_plan_seen" = 0 ] || { bk_bad=1; break; }
          bk_plan_seen=1
          ;;
        file)
          [ $# -eq 5 ] || { bk_bad=1; break; }
          case "$2" in ''|*[!0-9]*) bk_bad=1 ;; esac
          case "$3" in 0|1) ;; *) bk_bad=1 ;; esac
          case "$4" in -|[0-7][0-7][0-7]) ;; *) bk_bad=1 ;; esac
          ba_token_ok "$5" || bk_bad=1
          case "$bk_file_ids" in *"|$2|"*) bk_bad=1 ;; esac
          case "$bk_seen" in *"|$5|"*) bk_bad=1 ;; esac
          [ "$bk_bad" = 0 ] || break
          bk_file_ids="$bk_file_ids$2|"
          bk_seen="$bk_seen$5|"
          if [ "$3" = 1 ]; then
            [ -L "$bk_files_dir/$2" ] && bk_bad=1
            [ -f "$bk_files_dir/$2" ] || bk_bad=1
          fi
          bk_target=$(ba_path "$5") || bk_bad=1
          bk_target_dir=${bk_target%/*}
          if [ -L "$bk_target" ] || [ -L "$bk_target_dir" ] || [ ! -d "$bk_target_dir" ]; then bk_bad=1; fi
          if [ -e "$bk_target" ] && [ ! -f "$bk_target" ]; then bk_bad=1; fi
          [ "$bk_bad" = 0 ] || break
          if [ "$3" = 1 ]; then bk_action=overwrite; else bk_action=delete; fi
          bk_meta="$bk_meta"'AM_BK file '"$5"' '"$bk_action"'
'
          bk_count=$((bk_count + 1))
          ;;
        *) bk_bad=1; break ;;
      esac
    done < "$bk_manifest"
    set +f
  fi
  if [ "$bk_bad" = 0 ] && [ "$bk_header" = 1 ] && [ "$bk_id_seen" = 1 ] && [ "$bk_plan_seen" = 1 ] && [ "$bk_count" -gt 0 ]; then
    printf 'AM_BK backup %s\n' "$bk_id"
    printf '%s' "$bk_meta"
  else
    bk_skipped=$((bk_skipped + 1))
  fi
done
bk_end
AM_LIST_SH

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }
check() {
  desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
has() { printf '%s\n' "$1" | grep -qx -- "$2"; }
lacks() { ! printf '%s\n' "$1" | grep -q -- "$2"; }
perm() { ls -ld "$1" | cut -c1-10; }
newhome() { mktemp -d "$WORK/home.XXXXXX"; }
# run_apply <HOME> <backup id>：換掉 ba_id 那一行後在乾淨環境跑。
run_apply() {
  sed "s/^ba_id=.*/ba_id='$2'/" "$WORK/apply.sh" > "$WORK/a.sh"
  env -i HOME="$1" PATH=/usr/bin:/bin /bin/sh "$WORK/a.sh"
}
run_restore() {
  sed "s/^br_id=.*/br_id='$2'/" "$WORK/restore.sh" > "$WORK/r.sh"
  env -i HOME="$1" PATH=/usr/bin:/bin /bin/sh "$WORK/r.sh"
}
run_list() { env -i HOME="$1" PATH=/usr/bin:/bin /bin/sh "$WORK/list.sh"; }
snapshot() {
  find "$1" -print | LC_ALL=C sort | while IFS= read -r path; do
    rel=${path#"$1"}
    if [ -L "$path" ]; then kind=link; elif [ -d "$path" ]; then kind=dir; elif [ -f "$path" ]; then kind=file; else kind=other; fi
    case "$(uname -s)" in
      Darwin) meta=$(stat -f '%p:%m:%c' "$path") ;;
      *) meta=$(stat -c '%a:%Y:%Z' "$path") ;;
    esac
    data=
    [ ! -f "$path" ] || data=$(cksum < "$path" | awk '{print $1 ":" $2}')
    printf '%s|%s|%s|%s\n' "$rel" "$kind" "$meta" "$data"
  done
}
id_n() { printf '20261024T12%04dZ-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4' "$1"; }
ID1=$(id_n 0)
BK=.config/agents-manager/baseline-backup

printf '# %s\n' "$(uname -srm)"

# 1. symlink：目標、目標的目錄、備份根目錄。
H=$(newhome)
mkdir -p "$H/.codex"
printf 'a = 1\n' > "$H/outside.toml"
ln -s "$H/outside.toml" "$H/.codex/config.toml"
out=$(run_apply "$H" "$ID1")
check "symlink target refused" has "$out" 'AM_BA key 0 skipped symlink'
check "symlink target left untouched" test "$(cat "$H/outside.toml")" = 'a = 1'
H=$(newhome)
mkdir -p "$H/elsewhere"
printf 'a = 1\n' > "$H/elsewhere/config.toml"
ln -s "$H/elsewhere" "$H/.codex"
out=$(run_apply "$H" "$ID1")
check "symlink directory refused" has "$out" 'AM_BA key 0 skipped symlink'
H=$(newhome)
mkdir -p "$H/.codex" "$H/.config/agents-manager" "$H/bk"
printf 'a = 1\n' > "$H/.codex/config.toml"
ln -s "$H/bk" "$H/$BK"
out=$(run_apply "$H" "$ID1")
check "symlink backup root stops everything" has "$out" 'AM_BA error backup_failed'
check "file untouched when the backup fails" test "$(cat "$H/.codex/config.toml")" = 'a = 1'
check "nothing written through the backup symlink" test -z "$(ls "$H/bk")"

# 2. 只補不蓋：鍵已在（各種寫法、不同值）。
for existing in '[features]
hooks = false' '[features] # c
  hooks=false # x' 'features.hooks = false
[other]' 'features = { hooks = false }'; do
  H=$(newhome)
  mkdir -p "$H/.codex"
  printf '%s\n' "$existing" > "$H/.codex/config.toml"
  cp "$H/.codex/config.toml" "$H/orig"
  out=$(run_apply "$H" "$ID1")
  check "present key kept: $(printf %s "$existing" | head -1)" has "$out" 'AM_BA key 0 present'
  check "file byte-identical: $(printf %s "$existing" | head -1)" cmp -s "$H/orig" "$H/.codex/config.toml"
  check "no backup when nothing written" test ! -e "$H/$BK"
done

# 3. 缺鍵：補上、寫前備份、manifest、權限。
H=$(newhome)
mkdir -p "$H/.codex"
printf 'approval_policy = "AM-SECRET"\n\n[features]\nother = 1\n' > "$H/.codex/config.toml"
chmod 644 "$H/.codex/config.toml"
cp -p "$H/.codex/config.toml" "$H/orig"
out=$(run_apply "$H" "$ID1")
check "missing key applied" has "$out" 'AM_BA key 0 applied'
check "backup reported" has "$out" 'AM_BA backup'
check "output carries no path or content" lacks "$out" "$H"
check "output carries no file content" lacks "$out" 'AM-SECRET'
check "key added under [features]" test "$(sed -n '/^\[features\]$/{n;p;}' "$H/.codex/config.toml")" = 'hooks = true'
check "only one line added" test "$(grep -v '^hooks = true$' "$H/.codex/config.toml" | cmp -s - "$H/orig" && echo same)" = same
check "file mode kept 0644" test "$(perm "$H/.codex/config.toml")" = '-rw-r--r--'
check "no temp file left" test "$(ls -A "$H/.codex")" = 'config.toml'
B="$H/$BK/$ID1"
check "backup root 0700" test "$(perm "$H/$BK")" = 'drwx------'
check "backup dir 0700" test "$(perm "$B")" = 'drwx------'
check "backup files dir 0700" test "$(perm "$B/files")" = 'drwx------'
check "backup file 0600" test "$(perm "$B/files/1")" = '-rw-------'
check "manifest 0600" test "$(perm "$B/manifest")" = '-rw-------'
check "backup is the original bytes" cmp -s "$H/orig" "$B/files/1"
printf 'am-baseline-backup 1\nid %s\nplan_rev macverify\nfile 1 1 644 codex/config.toml\n' "$ID1" > "$WORK/want-manifest"
check "manifest records existed_before and mode" cmp -s "$WORK/want-manifest" "$B/manifest"
out=$(run_apply "$H" "$(id_n 1)")
check "second apply sees the key as present" has "$out" 'AM_BA key 0 present'

# 4. 還原：逐 byte 相同、權限回來。
out=$(run_restore "$H" "$ID1")
check "restore reports the file" has "$out" 'AM_BR file codex/config.toml restored'
check "restored byte-identical" cmp -s "$H/orig" "$H/.codex/config.toml"
check "restored mode 0644" test "$(perm "$H/.codex/config.toml")" = '-rw-r--r--'
check "restore output carries no path" lacks "$out" "$H"
out=$(run_restore "$H" "$(id_n 9)")
check "unknown backup is not_found" has "$out" 'AM_BR error not_found'

# 4a. 唯讀列舉：好備份列出 allowlist metadata；壞 manifest 與 symlink 略過，目錄快照不變。
H=$(newhome)
mkdir -p "$H/.codex" "$H/.claude" "$H/$BK/$(id_n 0)/files" "$H/$BK/$(id_n 1)/files"
printf 'current config\n' > "$H/.codex/config.toml"
printf 'backup config AM-SECRET\n' > "$H/$BK/$(id_n 0)/files/1"
printf 'am-baseline-backup 1\nid %s\nplan_rev macverify\nfile 1 1 644 codex/config.toml\nfile 2 0 - claude-default/settings.json\n' "$(id_n 0)" > "$H/$BK/$(id_n 0)/manifest"
printf 'am-baseline-backup 1\nid %s\nplan_rev macverify\nfile 1 1 600 /etc/passwd\n' "$(id_n 1)" > "$H/$BK/$(id_n 1)/manifest"
ln -s "$H/$BK/$(id_n 0)" "$H/$BK/$(id_n 2)"
before=$(snapshot "$H")
out=$(run_list "$H")
after=$(snapshot "$H")
check "backup listing has begin and end markers" has "$out" 'AM_BK end'
check "good backup and overwrite metadata listed" has "$out" "AM_BK backup $(id_n 0)"
check "backup file marked overwrite" has "$out" 'AM_BK file codex/config.toml overwrite'
check "absent backup file marked delete" has "$out" 'AM_BK file claude-default/settings.json delete'
check "bad manifest and symlink counted as skipped" has "$out" 'AM_BK skipped 2'
check "symlink backup refused from the restorable list" lacks "$out" "AM_BK backup $(id_n 2)"
check "listing output excludes file content and paths" lacks "$out" 'AM-SECRET'
check "listing output excludes HOME path" lacks "$out" "$H"
check "listing leaves directory, content, modes, and timestamps unchanged" test "$after" = "$before"

# 5. 不可信 manifest：任一行不對整個拒絕，一個檔都不寫。
for evil in 'file 2 1 600 /etc/passwd' 'file 2 1 600 claude-cc1/../../.ssh/settings.json' 'file 2 1 600 .ssh/id_rsa' 'exec rm -rf /' 'file 2 1 7777 claude-cc1/settings.json'; do
  H=$(newhome)
  mkdir -p "$H/.codex" "$H/.claude-cc1" "$H/$BK/$ID1/files"
  printf 'features.hooks = true\n' > "$H/.codex/config.toml"
  printf 'ORIGINAL\n' > "$H/$BK/$ID1/files/1"
  printf '{}\n' > "$H/$BK/$ID1/files/2"
  printf 'am-baseline-backup 1\nid %s\nfile 1 1 600 codex/config.toml\n%s\n' "$ID1" "$evil" > "$H/$BK/$ID1/manifest"
  out=$(run_restore "$H" "$ID1")
  check "untrusted manifest refused: $evil" has "$out" 'AM_BR error manifest_invalid'
  check "nothing restored: $evil" test "$(cat "$H/.codex/config.toml")" = 'features.hooks = true'
done
H=$(newhome)
mkdir -p "$H/.codex" "$H/$BK/$ID1/files"
printf 'features.hooks = true\n' > "$H/.codex/config.toml"
printf 'SECRET\n' > "$H/secret"
ln -s "$H/secret" "$H/$BK/$ID1/files/1"
printf 'am-baseline-backup 1\nid %s\nfile 1 1 600 codex/config.toml\n' "$ID1" > "$H/$BK/$ID1/manifest"
out=$(run_restore "$H" "$ID1")
check "symlinked backup file refused" has "$out" 'AM_BR error symlink'
check "nothing restored from a symlinked backup" test "$(cat "$H/.codex/config.toml")" = 'features.hooks = true'

# 6. 輪替：12 次之後只留最新 10 份，剛建的一定在，不是備份 id 形狀的不碰。
H=$(newhome)
mkdir -p "$H/.codex" "$H/$BK/keep-me"
n=1
while [ "$n" -le 12 ]; do
  printf 'a = 1\n' > "$H/.codex/config.toml"
  run_apply "$H" "$(id_n "$n")" > /dev/null
  n=$((n + 1))
done
check "ten backups kept" test "$(ls "$H/$BK" | grep -c '^20261024T')" -eq 10
check "the two oldest are gone" test ! -e "$H/$BK/$(id_n 1)" -a ! -e "$H/$BK/$(id_n 2)"
check "the newest is kept" test -d "$H/$BK/$(id_n 12)"
check "unrelated entries untouched" test -d "$H/$BK/keep-me"

printf '# %s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
