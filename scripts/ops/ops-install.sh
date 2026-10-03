#!/bin/bash
# 把 repo 裡「有變動、而且已經裝在 AGM 目錄」的 ops 腳本更新進去（只更新，不新增）。
#
#   ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run] [--force]
#
# 為什麼：已安裝的 ops 腳本沒有任何東西在換新（outbox-gc.sh 停在舊版、repo 修了卻沒生效）；`agm ops-sync --check` 只偵測。
# 這支補上「換新」，由 daemon-update-kick.sh 在 **旗標開著** 時於部署後叫（預設關，見 README「自動安裝」）。
#
# 規則（刻意窄）：
#   - 只看 `scripts/ops/install-manifest.tsv`（在 --ref 那版）列的檔、這個平台的列；來源用 `git show <ref>:<path>` 讀，不需要 checkout。
#   - 只換「安裝端已經有、內容跟 repo 不同」的檔。安裝端沒有的＝新檔，報 `not-installed`，第一次要人（或 AGM）手動裝。
#   - 安裝位置是 symlink 的不碰（`mv` 會把連結換成拷貝），報 `skipped`；對照表的安裝位置跑出 --dir（絕對路徑、`..`）報 `failed`、不寫。
#   - 排程 unit（`systemd/`、`LaunchAgents/`）不碰：換了還要 daemon-reload／launchctl，不是這支的事，報 `skipped`。
#   - 每支：新版先寫到同目錄暫存檔、**在暫存檔上自檢**（.sh → `bash -n`、.py → 語法編譯、.ts → 有 bun 就 `bun build`），
#     沒過就丟掉暫存檔、報 `failed`、安裝位置上的舊檔一個位元都沒動（不讓 cron／launchd 撞到沒驗過的新版）、其他支照裝；
#     過了才備份舊檔到 `<AGM>/ops-install-backups/<UTC 時間>/<安裝位置>`，再 `mv`（原子替換）。
#   - **手改過的不覆蓋**：安裝端的檔不是 repo 任何一版（`git log <ref> -- <來源>` 的哪個 blob 都對不上，同 `agm ops-sync --check` 的
#     `drift`）＝有人直接改了安裝檔，報 `drifted`、不動它、不算失敗。`--force` 才換（照樣先備份）。
#   - `--dry-run` 只列 `would-install`，什麼都不寫。
#
# 輸出一行一件事（installed／would-install／unchanged／not-installed／skipped／drifted／failed），最後一行 `changes=N failed=M drifted=K`。
# exit 0＝沒有失敗；1＝至少一支失敗（沒換成，舊檔原封不動）；2＝用法錯誤；3＝另一個 ops-install 正在裝（沒動任何檔）。
set -u

REPO=""; REF=""; DIR=""; PLATFORM=""; DRY=0; FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:-}"; shift 2 ;;
    --ref) REF="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --force) FORCE=1; shift ;;
    *) echo "用法：ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run] [--force]" >&2; exit 2 ;;
  esac
done
if [ -z "$REPO" ] || [ -z "$REF" ] || [ -z "$DIR" ]; then
  echo "用法：ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run] [--force]" >&2
  exit 2
fi
if [ -z "$PLATFORM" ]; then
  case "$(uname -s)" in Darwin) PLATFORM=darwin ;; *) PLATFORM=linux ;; esac
fi
GIT="${GIT_BIN:-git}"
COMMIT=$("$GIT" -C "$REPO" rev-parse --verify --quiet "${REF}^{commit}") || { echo "找不到 ${REF}" >&2; exit 2; }
MANIFEST=$("$GIT" -C "$REPO" show "${COMMIT}:scripts/ops/install-manifest.tsv" 2>/dev/null) || { echo "${REF} 沒有 scripts/ops/install-manifest.tsv" >&2; exit 2; }

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ops-install.XXXXXX")
LOCK="$DIR/ops-install.lock"; HAVE_LOCK=0
cleanup() { rm -rf "$TMP"; [ "$HAVE_LOCK" = 1 ] && rm -rf "$LOCK"; return 0; }
trap cleanup EXIT
# 同時只能有一個在裝（kick 與人手動、或兩輪 kick 重疊）：兩邊同時備份、換檔會互相蓋掉。鎖是 `<AGM>/ops-install.lock/`（裡面記 pid）；
# 握鎖的行程死了（被 SIGKILL，EXIT trap 沒跑）就算殘留，接手。--dry-run 什麼都不寫，不用鎖。
if [ "$DRY" = 0 ]; then
  mkdir -p "$DIR" 2>/dev/null
  _tries=0
  until mkdir "$LOCK" 2>/dev/null; do
    _holder=$(cat "$LOCK/pid" 2>/dev/null)
    case "$_holder" in ''|*[!0-9]*) _holder="" ;; esac
    if [ -n "$_holder" ] && kill -0 "$_holder" 2>/dev/null; then
      echo "busy 另一個 ops-install（pid ${_holder}）正在裝，這次不動任何檔" >&2; exit 3
    fi
    _tries=$((_tries + 1))
    [ "$_tries" -le 3 ] || { echo "busy 拿不到鎖 ${LOCK}" >&2; exit 3; }
    # 沒有 pid（剛建好還沒寫、或殘留）：等一下再看，真的沒人握才清掉。
    [ -z "$_holder" ] && sleep 1
    rm -rf "$LOCK" 2>/dev/null
  done
  echo $$ > "$LOCK/pid"; HAVE_LOCK=1
fi
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP="$DIR/ops-install-backups/$STAMP"
# 同一秒內再裝一次（不同 ref）：備份目錄不能共用，不然第二次的 `cp -p` 會蓋掉第一次留下的原始檔。
_n=1; while [ -e "$BACKUP" ]; do _n=$((_n + 1)); BACKUP="$DIR/ops-install-backups/$STAMP-$_n"; done
changes=0; failed=0

# 自檢（在暫存檔上做，種類看安裝位置的副檔名）。不在安裝目錄產生任何東西（py 不用 py_compile，它會寫 __pycache__）。
selfcheck() { # selfcheck <安裝位置> <要檢查的檔案>
  case "$1" in
    *.sh) bash -n "$2" 2>&1 ;;
    *.py) python3 -B -c 'import sys; compile(open(sys.argv[1]).read(), sys.argv[1], "exec")' "$2" 2>&1 ;;
    *.ts) if command -v bun >/dev/null 2>&1; then bun build --target=bun --outfile=/dev/null "$2" 2>&1 >/dev/null; fi ;;
    *) return 0 ;;
  esac
}

# 安裝端這份是不是 repo 在 --ref 之前（含）出現過的任何一版？不是＝有人手改過（drift，同 `agm ops-sync --check`）。
known_version() { # known_version <repo 來源> <安裝端檔案>
  _have=$("$GIT" hash-object "$2" 2>/dev/null) || return 1
  "$GIT" -C "$REPO" log --format=%H "$COMMIT" -- "$1" 2>/dev/null | while IFS= read -r _c; do
    [ "$("$GIT" -C "$REPO" rev-parse --verify --quiet "${_c}:$1" 2>/dev/null)" = "$_have" ] && { echo found; break; }
  done | grep -q found
}

# --dir 之下的父目錄也必須是實目錄。只檢查最終檔名會漏掉 bin -> /outside 這種 symlink，
# 安裝與孤兒 .new 清理都會沿它寫入／刪除 --dir 外的檔案。
has_symlink_parent() { # has_symlink_parent <relative target> → 0 是 symlink、1 否
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

# 上一次被 SIGKILL（EXIT trap 沒跑）會把 `<安裝位置>.new.<pid>` 留在安裝端；pid 已經不在的就是孤兒，清掉。
# 只看清單裡的安裝位置、名字必須是 `.new.<純數字>`、pid 還活著的不碰（別人正在裝）；--dry-run 不清。
if [ "$DRY" = 0 ]; then
  : > "$TMP/clean"
  printf '%s\n' "$MANIFEST" | while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue ;; esac
    # shellcheck disable=SC2086
    set -- $line
    [ $# -ge 2 ] || continue
    tgt="$2"; plat="${3:-}"
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

printf '%s\n' "$MANIFEST" | while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  # shellcheck disable=SC2086
  set -- $line
  [ $# -ge 2 ] || continue
  src="$1"; tgt="$2"; plat="${3:-}"
  [ -z "$plat" ] || [ "$plat" = "$PLATFORM" ] || continue
  case "$tgt" in
    systemd/*|LaunchAgents/*) echo "skipped ${tgt}（排程 unit 要 daemon-reload／launchctl，不自動換）"; continue ;;
  esac
  # 安裝位置只能在 --dir 底下：絕對路徑或 `..` 會寫到別人的目錄去（對照表打錯一個字就是這樣）。
  case "$tgt" in
    /*|..|../*|*/..|*/../*) echo "failed ${tgt}（安裝位置跑出 --dir，不寫）"; echo F >> "$TMP/results"; continue ;;
  esac
  if has_symlink_parent "$tgt"; then
    echo "failed ${tgt}（安裝位置父目錄是 symlink，不寫）"; echo F >> "$TMP/results"; continue
  fi
  dest="$DIR/$tgt"
  # symlink：使用者連到別處的（例如連到 repo 的腳本）。`mv` 會把連結換成一份拷貝、斷掉那條連結，所以不碰。
  if [ -L "$dest" ]; then echo "skipped ${tgt}（是 symlink，不換；連結指到哪就是哪版）"; continue; fi
  if [ ! -f "$dest" ]; then echo "not-installed ${tgt}（新檔，第一次要手動裝）"; continue; fi
  new="$TMP/new"
  if ! "$GIT" -C "$REPO" show "${COMMIT}:${src}" > "$new" 2>/dev/null; then
    echo "failed ${tgt}（${REF} 裡讀不到 ${src}）"; echo F >> "$TMP/results"; continue
  fi
  snapshot="$TMP/original"
  if ! cp "$dest" "$snapshot"; then
    echo "failed ${tgt}（讀不了安裝端的舊檔，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  if cmp -s "$new" "$snapshot"; then echo "unchanged $tgt"; continue; fi
  if [ "$FORCE" != 1 ] && ! known_version "$src" "$snapshot"; then
    echo "drifted ${tgt}（安裝端的檔不是 repo 任何一版，有人手改過；沒動它。要換就先看差在哪，或加 --force，舊檔照樣會備份）"
    echo D >> "$TMP/results"; continue
  fi
  echo C >> "$TMP/results"
  if [ "$DRY" = 1 ]; then echo "would-install $tgt"; continue; fi
  mode=$(stat -c %a "$dest" 2>/dev/null || stat -f %Lp "$dest" 2>/dev/null || echo 644)
  stage="$dest.new.$$"
  if ! { cp "$new" "$stage" && chmod "$mode" "$stage"; }; then
    rm -f "$stage"; echo "failed ${tgt}（寫不進去，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  # 先在暫存檔上自檢：沒過就丟掉，安裝位置上的舊檔沒被碰過。
  if ! err=$(selfcheck "$tgt" "$stage"); then
    rm -f "$stage"
    echo "failed ${tgt}（自檢沒過，沒換，舊版原封不動）：$(printf '%s' "$err" | head -2 | tr '\n' ' ')"
    echo F >> "$TMP/results"; continue
  fi
  # 自檢可能跑很久（例如 bun build）；期間若有人手改、刪掉或換成 symlink，不覆蓋那個新狀態。
  if has_symlink_parent "$tgt" || [ -L "$dest" ] || ! cmp -s "$snapshot" "$dest"; then
    rm -f "$stage"
    echo "drifted ${tgt}（安裝期間目的地改變，保留現況）"
    echo D >> "$TMP/results"; continue
  fi
  mkdir -p "$BACKUP/$(dirname "$tgt")"
  if ! cp -p "$dest" "$BACKUP/$tgt"; then rm -f "$stage"; echo "failed ${tgt}（備份不了，沒動它）"; echo F >> "$TMP/results"; continue; fi
  if ! mv -f "$stage" "$dest"; then
    rm -f "$stage"; echo "failed ${tgt}（換不進去，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  echo "installed ${tgt}（舊檔備份在 ${BACKUP}/${tgt}）"
done > "$TMP/lines"

cat "$TMP/lines"
changes=$(grep -c '^C$' "$TMP/results" 2>/dev/null || true)
failed=$(grep -c '^F$' "$TMP/results" 2>/dev/null || true)
drifted=$(grep -c '^D$' "$TMP/results" 2>/dev/null || true)
echo "changes=${changes:-0} failed=${failed:-0} drifted=${drifted:-0}"
# 兩個記錄檔（目前沒有程式讀它們，給人查「安裝端裝到哪一版了」用）：
#   ops-install.last         最近一次**整輪都成功**而且真的換了檔的 `<UTC 時間> <commit>`；
#   ops-install.last-failed  最近一次有失敗的 `<UTC 時間> <commit> failed=N`，下一次整輪成功就刪。
# 有失敗的那輪不更新 .last：部分失敗的安裝不能被說成「裝到這個 commit 了」。
if [ "$DRY" = 0 ]; then
  if [ "${failed:-0}" -gt 0 ]; then
    printf '%s %s failed=%s\n' "$STAMP" "$COMMIT" "$failed" > "$DIR/ops-install.last-failed"
  else
    rm -f "$DIR/ops-install.last-failed"
    [ "${changes:-0}" -gt 0 ] && printf '%s %s\n' "$STAMP" "$COMMIT" > "$DIR/ops-install.last"
  fi
fi
[ "${failed:-0}" = 0 ]
