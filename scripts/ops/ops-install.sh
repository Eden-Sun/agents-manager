#!/bin/bash
# 把 repo 裡「有變動、而且已經裝在 AGM 目錄」的 ops 腳本更新進去（只更新，不新增）。
#
#   ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run]
#
# 為什麼：已安裝的 ops 腳本沒有任何東西在換新（outbox-gc.sh 停在舊版、repo 修了卻沒生效）；`agm ops-sync --check` 只偵測。
# 這支補上「換新」，由 daemon-update-kick.sh 在 **旗標開著** 時於部署後叫（預設關，見 README「自動安裝」）。
#
# 規則（刻意窄）：
#   - 只看 `scripts/ops/install-manifest.tsv`（在 --ref 那版）列的檔、這個平台的列；來源用 `git show <ref>:<path>` 讀，不需要 checkout。
#   - 只換「安裝端已經有、內容跟 repo 不同」的檔。安裝端沒有的＝新檔，報 `not-installed`，第一次要人（或 AGM）手動裝。
#   - 排程 unit（`systemd/`、`LaunchAgents/`）不碰：換了還要 daemon-reload／launchctl，不是這支的事，報 `skipped`。
#   - 每支：先備份舊檔到 `<AGM>/ops-install-backups/<UTC 時間>/<安裝位置>`，寫到同目錄暫存檔再 `mv`（原子替換），
#     最後自檢（.sh → `bash -n`、.py → 語法編譯、.ts → 有 bun 就 `bun build`），沒過就把備份放回去、報 `failed`、其他支照裝。
#   - `--dry-run` 只列 `would-install`，什麼都不寫。
#
# 輸出一行一件事（installed／would-install／unchanged／not-installed／skipped／failed），最後一行 `changes=N failed=M`。
# exit 0＝沒有失敗；1＝至少一支失敗（已還原）；2＝用法錯誤。
set -u

REPO=""; REF=""; DIR=""; PLATFORM=""; DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:-}"; shift 2 ;;
    --ref) REF="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    *) echo "用法：ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run]" >&2; exit 2 ;;
  esac
done
if [ -z "$REPO" ] || [ -z "$REF" ] || [ -z "$DIR" ]; then
  echo "用法：ops-install.sh --repo <git 目錄> --ref <rev> --dir <AGM 目錄> [--platform linux|darwin] [--dry-run]" >&2
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

# 自檢安裝好的檔。不在安裝目錄產生任何東西（py 不用 py_compile，它會寫 __pycache__）。
selfcheck() { # selfcheck <path>
  case "$1" in
    *.sh) bash -n "$1" 2>&1 ;;
    *.py) python3 -B -c 'import sys; compile(open(sys.argv[1]).read(), sys.argv[1], "exec")' "$1" 2>&1 ;;
    *.ts) if command -v bun >/dev/null 2>&1; then bun build --target=bun --outfile=/dev/null "$1" 2>&1 >/dev/null; fi ;;
    *) return 0 ;;
  esac
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
  dest="$DIR/$tgt"
  if [ ! -f "$dest" ]; then echo "not-installed ${tgt}（新檔，第一次要手動裝）"; continue; fi
  new="$TMP/new"
  if ! "$GIT" -C "$REPO" show "${COMMIT}:${src}" > "$new" 2>/dev/null; then
    echo "failed ${tgt}（${REF} 裡讀不到 ${src}）"; echo F >> "$TMP/results"; continue
  fi
  if cmp -s "$new" "$dest"; then echo "unchanged $tgt"; continue; fi
  echo C >> "$TMP/results"
  if [ "$DRY" = 1 ]; then echo "would-install $tgt"; continue; fi
  mkdir -p "$BACKUP/$(dirname "$tgt")"
  if ! cp -p "$dest" "$BACKUP/$tgt"; then echo "failed ${tgt}（備份不了，沒動它）"; echo F >> "$TMP/results"; continue; fi
  mode=$(stat -c %a "$dest" 2>/dev/null || stat -f %Lp "$dest" 2>/dev/null || echo 644)
  stage="$dest.new.$$"
  if ! { cp "$new" "$stage" && chmod "$mode" "$stage" && mv -f "$stage" "$dest"; }; then
    rm -f "$stage"; echo "failed ${tgt}（寫不進去，沒動它）"; echo F >> "$TMP/results"; continue
  fi
  if err=$(selfcheck "$dest"); then
    echo "installed ${tgt}（舊檔備份在 ${BACKUP}/${tgt}）"
  else
    # 還原：備份放回去（同樣先寫暫存檔再 mv），壞掉的新版不留在安裝端。
    cp -p "$BACKUP/$tgt" "$stage" && mv -f "$stage" "$dest"
    echo "failed ${tgt}（自檢沒過，已還原舊版）：$(printf '%s' "$err" | head -2 | tr '\n' ' ')"
    echo F >> "$TMP/results"
  fi
done > "$TMP/lines"

cat "$TMP/lines"
changes=$(grep -c '^C$' "$TMP/results" 2>/dev/null || true)
failed=$(grep -c '^F$' "$TMP/results" 2>/dev/null || true)
echo "changes=${changes:-0} failed=${failed:-0}"
if [ "$DRY" = 0 ] && [ "${changes:-0}" -gt 0 ]; then
  printf '%s %s\n' "$STAMP" "$COMMIT" > "$DIR/ops-install.last"
fi
[ "${failed:-0}" = 0 ]
