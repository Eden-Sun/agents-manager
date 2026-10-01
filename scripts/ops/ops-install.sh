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
# exit 0＝沒有失敗；1＝至少一支失敗（已還原）；2＝用法錯誤。
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
trap 'rm -rf "$TMP"' EXIT
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP="$DIR/ops-install-backups/$STAMP"
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
  dest="$DIR/$tgt"
  # symlink：使用者連到別處的（例如連到 repo 的腳本）。`mv` 會把連結換成一份拷貝、斷掉那條連結，所以不碰。
  if [ -L "$dest" ]; then echo "skipped ${tgt}（是 symlink，不換；連結指到哪就是哪版）"; continue; fi
  if [ ! -f "$dest" ]; then echo "not-installed ${tgt}（新檔，第一次要手動裝）"; continue; fi
  new="$TMP/new"
  if ! "$GIT" -C "$REPO" show "${COMMIT}:${src}" > "$new" 2>/dev/null; then
    echo "failed ${tgt}（${REF} 裡讀不到 ${src}）"; echo F >> "$TMP/results"; continue
  fi
  if cmp -s "$new" "$dest"; then echo "unchanged $tgt"; continue; fi
  if [ "$FORCE" != 1 ] && ! known_version "$src" "$dest"; then
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
if [ "$DRY" = 0 ] && [ "${changes:-0}" -gt 0 ]; then
  printf '%s %s\n' "$STAMP" "$COMMIT" > "$DIR/ops-install.last"
fi
[ "${failed:-0}" = 0 ]
