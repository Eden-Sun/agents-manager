#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/ops-install.sh"
GITBIN=$(command -v git)
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

setup() {
  ROOT=$(mktemp -d)
  REPO="$ROOT/repo"; DIR="$ROOT/agm"; OUT="$ROOT/out"
  "$GITBIN" init -q -b main "$REPO"
  "$GITBIN" -C "$REPO" config user.email t@t; "$GITBIN" -C "$REPO" config user.name t
  mkdir -p "$REPO/scripts/ops/systemd" "$DIR/bin"
  cat > "$REPO/scripts/ops/install-manifest.tsv" <<'M'
# 測試用對照表
scripts/ops/a-kick.sh    bin/a-kick.sh
scripts/ops/b-tool.py    bin/b-tool.py
scripts/ops/c-task.md    c-task.md
scripts/ops/new-one.sh   bin/new-one.sh
scripts/ops/mac-only.sh  bin/mac-only.sh  darwin
scripts/ops/linux-only.sh  bin/linux-only.sh  linux
scripts/ops/systemd/x.timer  systemd/x.timer  linux
M
  printf '#!/bin/bash\necho a-v1\n' > "$REPO/scripts/ops/a-kick.sh"
  printf 'print("b-v1")\n' > "$REPO/scripts/ops/b-tool.py"
  printf 'task v1\n' > "$REPO/scripts/ops/c-task.md"
  printf '#!/bin/bash\necho new\n' > "$REPO/scripts/ops/new-one.sh"
  printf '#!/bin/bash\necho mac\n' > "$REPO/scripts/ops/mac-only.sh"
  printf '#!/bin/bash\necho linux\n' > "$REPO/scripts/ops/linux-only.sh"
  printf '[Timer]\nOnCalendar=daily\n' > "$REPO/scripts/ops/systemd/x.timer"
  "$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m v1
  install -m 755 "$REPO/scripts/ops/a-kick.sh" "$DIR/bin/a-kick.sh"
  install -m 755 "$REPO/scripts/ops/b-tool.py" "$DIR/bin/b-tool.py"
  install -m 644 "$REPO/scripts/ops/c-task.md" "$DIR/c-task.md"
  install -m 755 "$REPO/scripts/ops/linux-only.sh" "$DIR/bin/linux-only.sh"
  mkdir -p "$DIR/systemd"; printf '[Timer]\nOnCalendar=hourly\n' > "$DIR/systemd/x.timer"
  printf 'unversioned\n' > "$DIR/bin/not-in-manifest.sh"
}
teardown() { rm -rf "$ROOT"; }
bump() {
  printf '%s' "$2" > "$REPO/scripts/ops/$1"
  "$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m "bump $1"
}
run() { bash "$SCRIPT" --repo "$REPO" --ref HEAD --dir "$DIR" --platform linux "$@" >"$OUT" 2>&1; echo $?; }

setup
equals "一致時 exit 0" "$(run)" "0"
check "changes=0" "changes=0" "$OUT"
gone "沒有備份目錄" "$DIR/ops-install-backups"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
bump c-task.md $'task v2\n'
bump linux-only.sh $'#!/bin/bash\necho linux-new\n'
bump systemd/x.timer $'[Timer]\nOnCalendar=weekly\n'
equals "更新 exit 0" "$(run)" "0"
check "a-kick 裝了" "installed bin/a-kick.sh" "$OUT"
check "task md 裝了" "installed c-task.md" "$OUT"
equals "a-kick 內容是 v2" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v2"
equals "a-kick 還是可執行" "$([ -x "$DIR/bin/a-kick.sh" ] && echo x)" "x"
equals "md 不是可執行" "$([ -x "$DIR/c-task.md" ] && echo x || echo n)" "n"
B=$(ls -d "$DIR"/ops-install-backups/*/ | head -1)
equals "舊的 a-kick 在備份裡" "$(sed -n 2p "${B}bin/a-kick.sh")" "echo a-v1"
equals "舊的 task 在備份裡" "$(cat "${B}c-task.md")" "task v1"
equals "b-tool 沒變不備份" "$([ -e "${B}bin/b-tool.py" ] && echo yes || echo no)" "no"
check "新檔不自動裝（要第一次手動）" "not-installed bin/new-one.sh" "$OUT"
gone "new-one.sh 沒被建立" "$DIR/bin/new-one.sh"
gone "darwin 的列在 linux 不動" "$DIR/bin/mac-only.sh"
equals "排程 unit 不動（要 daemon-reload 與核准）" "$(sed -n 2p "$DIR/systemd/x.timer")" "OnCalendar=hourly"
check "排程 unit 報成跳過" "skipped systemd/x.timer" "$OUT"
equals "清單外的檔不動" "$(cat "$DIR/bin/not-in-manifest.sh")" "unversioned"
equals "linux-only 也更新" "$(sed -n 2p "$DIR/bin/linux-only.sh")" "echo linux-new"
check "彙總 3 個、0 失敗" "changes=3 failed=0" "$OUT"
gone "沒有留下暫存檔" "$DIR/bin/a-kick.sh.new"
equals "沒有留下 .new 殘檔" "$(ls "$DIR/bin" | grep -c '\.new' || true)" "0"
check "記下裝的是哪個 commit" "$("$GITBIN" -C "$REPO" rev-parse HEAD | cut -c1-8)" "$DIR/ops-install.last"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
equals "dry-run exit 0" "$(run --dry-run)" "0"
check "列出會裝" "would-install bin/a-kick.sh" "$OUT"
check "changes=1" "changes=1" "$OUT"
equals "安裝端沒動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
gone "沒有備份目錄" "$DIR/ops-install-backups"
gone "沒有 last 記錄" "$DIR/ops-install.last"
teardown

setup
bump a-kick.sh $'#!/bin/bash\nif then fi (\n'
bump b-tool.py $'print("b-v2")\n'
bump c-task.md $'task v2\n'
equals "有自檢失敗 exit 1" "$(run)" "1"
check "點名壞的那支" "failed bin/a-kick.sh" "$OUT"
equals "壞的那支還原成舊檔" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
equals "還原後還能執行" "$(bash -n "$DIR/bin/a-kick.sh" && echo ok)" "ok"
equals "py 的新版照裝" "$(cat "$DIR/bin/b-tool.py")" 'print("b-v2")'
equals "md 的新版照裝" "$(cat "$DIR/c-task.md")" "task v2"
check "彙總 2 個裝了、1 失敗" "changes=3 failed=1" "$OUT"
teardown

setup
bump b-tool.py $'def (:\n'
equals "py 語法壞 exit 1" "$(run)" "1"
check "點名 py" "failed bin/b-tool.py" "$OUT"
equals "py 還原" "$(cat "$DIR/bin/b-tool.py")" 'print("b-v1")'
gone "沒有 __pycache__" "$DIR/bin/__pycache__"
teardown

setup
printf 'scripts/ops/ghost.sh  bin/ghost.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m ghost
printf '#!/bin/bash\n' > "$DIR/bin/ghost.sh"
equals "來源缺 exit 1" "$(run)" "1"
check "點名" "failed bin/ghost.sh" "$OUT"
teardown

setup
equals "沒帶 --dir exit 2" "$(bash "$SCRIPT" --repo "$REPO" >"$OUT" 2>&1; echo $?)" "2"
run_missing_value() {
  python3 - "$SCRIPT" "$1" <<'PY'
import subprocess, sys
try:
    result = subprocess.run(["bash", sys.argv[1], sys.argv[2]], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, timeout=2)
except subprocess.TimeoutExpired:
    print("timeout")
else:
    print(result.returncode)
PY
}
for _option in --repo --ref --dir --platform; do
  equals "${_option} 缺值快速回 usage" "$(run_missing_value "$_option")" "2"
done
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
bump c-task.md $'task v2\n'
printf '#!/bin/bash\necho a-hand-edited\n' > "$DIR/bin/a-kick.sh"
equals "有手改的檔：不算失敗 exit 0" "$(run)" "0"
check "點名 drifted" "drifted bin/a-kick.sh" "$OUT"
equals "手改過的檔原封不動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-hand-edited"
equals "沒手改的照裝" "$(cat "$DIR/c-task.md")" "task v2"
check "彙總帶 drifted 數" "changes=1 failed=0 drifted=1" "$OUT"
run --dry-run >/dev/null
equals "dry-run 也不把它算進會裝的" "$(grep -c '^would-install bin/a-kick.sh' "$OUT" || true)" "0"
equals "--force 才換" "$(run --force)" "0"
equals "強制換成 v2" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v2"
B=$(ls -d "$DIR"/ops-install-backups/*/bin/a-kick.sh | tail -1)
equals "手改的版本在備份裡" "$(sed -n 2p "$B")" "echo a-hand-edited"
teardown

setup
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
printf 'scripts/ops/t-tool.ts  bin/t-tool.ts\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf 'console.log("t-v1")\n' > "$REPO/scripts/ops/t-tool.ts"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m ts
install -m 755 "$REPO/scripts/ops/t-tool.ts" "$DIR/bin/t-tool.ts"
bump t-tool.ts $'console.log("t-v2")\n'
cat > "$FAKEBIN/bun" <<B
#!/bin/sh
cat "$DIR/bin/t-tool.ts" > "$ROOT/live-during-selfcheck"
echo 'build failed' >&2
exit 1
B
chmod +x "$FAKEBIN/bun"
equals "ts 自檢失敗 exit 1" "$(PATH="$FAKEBIN:$PATH" run)" "1"
equals "自檢那一刻安裝位置還是舊檔" "$(cat "$ROOT/live-during-selfcheck")" 'console.log("t-v1")'
equals "失敗後還是舊檔" "$(cat "$DIR/bin/t-tool.ts")" 'console.log("t-v1")'
check "點名" "failed bin/t-tool.ts" "$OUT"
teardown

setup
printf 'scripts/ops/t-tool.ts  bin/t-tool.ts\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf 'console.log("t-v1")\n' > "$REPO/scripts/ops/t-tool.ts"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m ts
install -m 755 "$REPO/scripts/ops/t-tool.ts" "$DIR/bin/t-tool.ts"
bump t-tool.ts $'console.log("t-v2")\n'
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/bun" <<'BUN'
#!/bin/sh
printf 'console.log("hand-edited during install")\n' > "$DEST"
exit 0
BUN
chmod +x "$FAKEBIN/bun"
equals "手改 race 不讓安裝器覆蓋 exit 0" "0" "$(DEST="$DIR/bin/t-tool.ts" PATH="$FAKEBIN:$PATH" run)"
equals "自檢期間手改的檔保留" 'console.log("hand-edited during install")' "$(cat "$DIR/bin/t-tool.ts")"
check "自檢期間手改有回報 drifted" "drifted bin/t-tool.ts" "$OUT"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/cp" <<'CP'
#!/bin/sh
/bin/cp "$@" || exit $?
case "$*" in
  *ops-install-backups*) printf '#!/bin/bash\necho hand-edited-during-backup\n' > "$DEST" ;;
esac
CP
chmod +x "$FAKEBIN/cp"
equals "備份期間手改仍回報成功 exit 0" "0" "$(DEST="$DIR/bin/a-kick.sh" PATH="$FAKEBIN:$PATH" run)"
equals "備份期間手改的檔保留" "echo hand-edited-during-backup" "$(sed -n 2p "$DIR/bin/a-kick.sh")"
check "備份期間手改有回報 drifted" "drifted bin/a-kick.sh" "$OUT"
teardown

setup
printf 'scripts/ops/t-tool.ts  bin/t-tool.ts\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf 'console.log("t-v1")\n' > "$REPO/scripts/ops/t-tool.ts"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m ts
install -m 755 "$REPO/scripts/ops/t-tool.ts" "$DIR/bin/t-tool.ts"
bump t-tool.ts $'console.log("t-v2")\n'
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/bun" <<'BUN'
#!/bin/sh
for last; do :; done
case "$(basename "$last")" in
  *.ts) exit 0 ;;
  *) echo "error: unexpected file name $(basename "$last") (bun picks its loader by extension)" >&2; exit 1 ;;
esac
BUN
chmod +x "$FAKEBIN/bun"
equals "ts 自檢的檔名以 .ts 結尾：exit 0" "$(PATH="$FAKEBIN:$PATH" run)" "0"
check "ts 裝上了" "installed bin/t-tool.ts" "$OUT"
equals "新內容換進去" "$(cat "$DIR/bin/t-tool.ts")" 'console.log("t-v2")'
gone "自檢暫存檔沒有留在安裝目錄" "$DIR/bin/t-tool.ts.new.$$"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
cp "$DIR/bin/a-kick.sh" "$ROOT/elsewhere.sh"; rm "$DIR/bin/a-kick.sh"; ln -s "$ROOT/elsewhere.sh" "$DIR/bin/a-kick.sh"
equals "symlink：exit 0" "$(run)" "0"
check "報 skipped" "skipped bin/a-kick.sh" "$OUT"
equals "連結還在" "$([ -L "$DIR/bin/a-kick.sh" ] && echo link)" "link"
equals "連結指到的檔沒動" "$(sed -n 2p "$ROOT/elsewhere.sh")" "echo a-v1"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
mkdir -p "$ROOT/outside"
mv "$DIR/bin" "$ROOT/outside/bin"
ln -s "$ROOT/outside/bin" "$DIR/bin"
DEAD=$(sh -c 'echo $$')
printf 'outside orphan' > "$ROOT/outside/bin/a-kick.sh.new.$DEAD"
equals "父目錄 symlink 的目的地回報失敗" "1" "$(run)"
check "父目錄 symlink 有點名" "symlink" "$OUT"
equals "父目錄 symlink 不能覆蓋外部檔" "echo a-v1" "$(sed -n 2p "$ROOT/outside/bin/a-kick.sh")"
equals "父目錄 symlink 不能刪外部孤兒檔" "outside orphan" "$(cat "$ROOT/outside/bin/a-kick.sh.new.$DEAD")"
teardown

setup
printf '#!/bin/bash\necho outside\n' > "$ROOT/escape.sh"
printf 'scripts/ops/esc.sh  ../escape.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf '#!/bin/bash\necho esc-v1\n' > "$REPO/scripts/ops/esc.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m esc
equals "路徑跑出去 exit 1" "$(run)" "1"
check "點名" "failed ../escape.sh" "$OUT"
equals "目錄外的檔沒動" "$(sed -n 2p "$ROOT/escape.sh")" "echo outside"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
equals "第一次裝 exit 0" "$(run)" "0"
bump a-kick.sh $'#!/bin/bash\necho a-v3\n'
equals "第二次裝 exit 0" "$(run)" "0"
equals "兩份備份都在（v1 沒被 v2 蓋掉）" "$(cat "$DIR"/ops-install-backups/*/bin/a-kick.sh | grep -c 'echo a-v1\|echo a-v2')" "2"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
sleep 30 & HOLDER=$!
mkdir "$DIR/ops-install.lock"; echo "$HOLDER" > "$DIR/ops-install.lock/pid"
equals "鎖被活著的行程握著：exit 3" "$(run)" "3"
check "說有人在裝" "busy" "$OUT"
equals "什麼都沒動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
kill "$HOLDER" 2>/dev/null; wait "$HOLDER" 2>/dev/null
equals "殘留的鎖（pid 已死）：接手照裝" "$(run)" "0"
equals "裝上了" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v2"
gone "裝完放掉鎖" "$DIR/ops-install.lock"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
printf 'preserve outside guard target\n' > "$ROOT/guard-target"
ln -s "$ROOT/guard-target" "$DIR/ops-install.lock.guard"
equals "guard symlink 不可取得鎖 exit 3" "$(run)" "3"
equals "guard symlink 目標原封不動" "$(cat "$ROOT/guard-target")" "preserve outside guard target"
equals "guard symlink 下安裝檔不動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
FIRST_CREATED="$ROOT/first-created" ALLOW_FIRST="$ROOT/allow-first" SECOND_CREATED="$ROOT/second-created"
export FIRST_CREATED ALLOW_FIRST SECOND_CREATED
cat > "$FAKEBIN/mkdir" <<'MKDIR'
#!/bin/sh
case "$*" in
  *ops-install.lock)
    /bin/mkdir "$@" || exit $?
    if [ ! -e "$FIRST_CREATED" ]; then
      : > "$FIRST_CREATED"
      while [ ! -e "$ALLOW_FIRST" ]; do /bin/sleep 0.02; done
    else
      : > "$SECOND_CREATED"
    fi
    ;;
  *) exec /bin/mkdir "$@" ;;
esac
MKDIR
chmod +x "$FAKEBIN/mkdir"
PATH="$FAKEBIN:$PATH" bash "$SCRIPT" --repo "$REPO" --ref HEAD --dir "$DIR" --platform linux >"$ROOT/first.out" 2>&1 & FIRST=$!
_wait=0
while [ ! -e "$FIRST_CREATED" ] && [ "$_wait" -lt 250 ]; do sleep 0.02; _wait=$((_wait + 1)); done
if [ ! -e "$FIRST_CREATED" ]; then
  echo "FAIL - 第一個 runner 沒建鎖"; FAIL=$((FAIL + 1))
  : > "$ALLOW_FIRST"; wait "$FIRST" 2>/dev/null
else
  PATH="$FAKEBIN:$PATH" bash "$SCRIPT" --repo "$REPO" --ref HEAD --dir "$DIR" --platform linux >"$ROOT/second.out" 2>&1 & SECOND=$!
  _wait=0
  while [ ! -e "$SECOND_CREATED" ] && kill -0 "$SECOND" 2>/dev/null && [ "$_wait" -lt 250 ]; do sleep 0.02; _wait=$((_wait + 1)); done
  : > "$ALLOW_FIRST"
  wait "$FIRST" 2>/dev/null; FIRST_RC=$?
  if wait "$SECOND" 2>/dev/null; then SECOND_RC=0; else SECOND_RC=$?; fi
  equals "鎖尚未寫 owner 時第二 runner exit 3" "$SECOND_RC" "3"
  gone "第二 runner 沒有重建鎖目錄" "$ROOT/second-created"
  check "第一 runner 完成更新" "installed bin/a-kick.sh" "$ROOT/first.out"
fi
teardown

setup
cat > "$REPO/scripts/ops/a-kick.sh" <<K
#!/bin/bash
bash "$SCRIPT" --repo "$REPO" --ref HEAD --dir "$DIR" --platform linux > "$ROOT/inner.out" 2>&1
echo after-install-1
echo after-install-2
echo "padding padding padding padding padding padding padding padding padding padding"
echo after-install-3
K
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m self-v1
install -m 755 "$REPO/scripts/ops/a-kick.sh" "$DIR/bin/a-kick.sh"
printf '#!/bin/bash\necho new-version-short\n' > "$REPO/scripts/ops/a-kick.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m self-v2
bash "$DIR/bin/a-kick.sh" >"$OUT" 2>&1
check "自己被換掉之後，後面的行照跑（1）" "after-install-1" "$OUT"
check "自己被換掉之後，後面的行照跑（3）" "after-install-3" "$OUT"
check_no "沒有讀到新檔的內容" "new-version-short" "$OUT"
equals "換上去的是新版" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo new-version-short"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
sleep 30 & LIVE=$!
DEAD=$(sh -c 'echo $$')
printf 'half' > "$DIR/bin/a-kick.sh.new.$DEAD"
printf 'half' > "$DIR/c-task.md.new.$DEAD"
printf 'live' > "$DIR/bin/a-kick.sh.new.$LIVE"
printf 'mine' > "$DIR/bin/a-kick.sh.new.notapid"
printf 'other' > "$DIR/bin/not-in-manifest.sh.new.$DEAD"
equals "裝完 exit 0" "$(run)" "0"
gone "死掉的 pid 留下的暫存檔清掉（bin）" "$DIR/bin/a-kick.sh.new.$DEAD"
gone "死掉的 pid 留下的暫存檔清掉（根目錄的檔）" "$DIR/c-task.md.new.$DEAD"
equals "還活著的 pid 的暫存檔不碰" "$(cat "$DIR/bin/a-kick.sh.new.$LIVE")" "live"
equals "名字不是 .new.<數字> 的不碰" "$(cat "$DIR/bin/a-kick.sh.new.notapid")" "mine"
equals "清單外的檔的暫存檔不碰" "$(cat "$DIR/bin/not-in-manifest.sh.new.$DEAD")" "other"
check "有說清了幾個" "cleaned 2" "$OUT"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
DEAD=$(sh -c 'echo $$')
printf 'half' > "$DIR/bin/a-kick.sh.new.$DEAD"
run --dry-run >/dev/null
equals "dry-run 不清暫存檔" "$(cat "$DIR/bin/a-kick.sh.new.$DEAD")" "half"
teardown

setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
equals "第一次整輪成功 exit 0" "$(run)" "0"
LAST1=$(cat "$DIR/ops-install.last")
gone "成功沒有 last-failed" "$DIR/ops-install.last-failed"
bump a-kick.sh $'#!/bin/bash\nif then fi (\n'
bump c-task.md $'task v3\n'
equals "有一支自檢失敗 exit 1" "$(run)" "1"
equals "失敗那輪不更新 .last" "$(cat "$DIR/ops-install.last")" "$LAST1"
check "另記 .last-failed（commit）" "$("$GITBIN" -C "$REPO" rev-parse HEAD | cut -c1-8)" "$DIR/ops-install.last-failed"
check "另記 .last-failed（失敗數）" "failed=1" "$DIR/ops-install.last-failed"
bump a-kick.sh $'#!/bin/bash\necho a-v4\n'
equals "修好之後整輪成功" "$(run)" "0"
check ".last 換成這次的 commit" "$("$GITBIN" -C "$REPO" rev-parse HEAD | cut -c1-8)" "$DIR/ops-install.last"
gone "成功之後清掉 .last-failed" "$DIR/ops-install.last-failed"
teardown

setup
bump a-kick.sh $'#!/bin/bash\nif then fi (\n'
equals "第一次就失敗 exit 1" "$(run)" "1"
gone "沒有 .last" "$DIR/ops-install.last"
check "有 .last-failed" "failed=1" "$DIR/ops-install.last-failed"
teardown

setup
printf 'scripts/ops/z-gc.sh  bin/z-gc.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf '#!/bin/zsh\necho z-v1\n' > "$REPO/scripts/ops/z-gc.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m z1
install -m 755 "$REPO/scripts/ops/z-gc.sh" "$DIR/bin/z-gc.sh"
bump z-gc.sh $'#!/bin/zsh\nfor d in "$DIR"/*(N/); do :; done\necho z-v2\n'
equals "zsh 腳本通過 exit 0" "$(run)" "0"
check "zsh 腳本裝了" "installed bin/z-gc.sh" "$OUT"
equals "zsh 腳本內容是 v2" "$(sed -n 3p "$DIR/bin/z-gc.sh")" "echo z-v2"
teardown

setup
printf 'scripts/ops/z-gc.sh  bin/z-gc.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf '#!/usr/bin/env zsh\necho z-v1\n' > "$REPO/scripts/ops/z-gc.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m z1
install -m 755 "$REPO/scripts/ops/z-gc.sh" "$DIR/bin/z-gc.sh"
bump z-gc.sh $'#!/usr/bin/env zsh\nfor d in "$DIR"/*(N/); do :; done\necho z-v2\n'
FAKEBIN="$ROOT/fakebin"; mkdir -p "$FAKEBIN"
for _entry in $(echo "$PATH" | tr ':' ' '); do
  [ -d "$_entry" ] || continue
  for _f in "$_entry"/*; do
    [ -x "$_f" ] || continue
    _b="${_f##*/}"
    [ "$_b" = "zsh" ] && continue
    [ -e "$FAKEBIN/$_b" ] || ln -s "$_f" "$FAKEBIN/$_b" 2>/dev/null || true
  done
done
# canary-gap: 測試環境刻意排除 zsh 直譯器以驗證 skipped 行為；AM_CANARY_DIR 仍保留以維持破壞性指令護欄
equals "沒裝 zsh exit 0" "$(PATH="$FAKEBIN${AM_CANARY_DIR:+:$AM_CANARY_DIR}" run)" "0"
check "zsh 檔報 skipped" "skipped bin/z-gc.sh" "$OUT"
check_no "沒有 failed" "failed bin/z-gc.sh" "$OUT"
equals "舊版原封不動" "$(sed -n 2p "$DIR/bin/z-gc.sh")" "echo z-v1"
gone "沒有留下暫存檔" "$DIR/bin/z-gc.sh.new"
teardown

setup
printf 'scripts/ops/z-bad.sh  bin/z-bad.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf '#!/bin/zsh\necho z-v1\n' > "$REPO/scripts/ops/z-bad.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m zbad
install -m 755 "$REPO/scripts/ops/z-bad.sh" "$DIR/bin/z-bad.sh"
bump a-kick.sh $'#!/bin/bash\nif then fi (\n'
bump z-bad.sh $'#!/bin/zsh\nif then fi (\n'
equals "壞腳本 exit 1" "$(run)" "1"
check "壞的 bash 腳本報 failed" "failed bin/a-kick.sh" "$OUT"
check "壞的 zsh 腳本報 failed" "failed bin/z-bad.sh" "$OUT"
equals "bash 舊檔原封不動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
equals "zsh 舊檔原封不動" "$(sed -n 2p "$DIR/bin/z-bad.sh")" "echo z-v1"
teardown

setup
printf 'scripts/ops/lib-common.sh  bin/lib-common.sh  shared\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf 'echo lib\n' > "$REPO/scripts/ops/lib-common.sh"
bump a-kick.sh $'#!/bin/bash\n. "${0%/*}/lib-common.sh"\necho a-v2\n'
equals "dry-run exit 0" "$(run --dry-run)" "0"
check "dry-run 列 lib would-install" "would-install bin/lib-common.sh" "$OUT"
gone "dry-run 沒寫 lib" "$DIR/bin/lib-common.sh"
equals "shared 安裝 exit 0" "$(run)" "0"
check "lib 裝了" "installed bin/lib-common.sh" "$OUT"
check "新 kick 也裝了" "installed bin/a-kick.sh" "$OUT"
[ "$(grep -n 'bin/lib-common.sh' "$OUT" | head -1 | cut -d: -f1)" -lt "$(grep -n 'installed bin/a-kick.sh' "$OUT" | head -1 | cut -d: -f1)" ] && { echo "ok   - lib 先於 kick"; PASS=$((PASS + 1)); } || { echo "FAIL - lib 沒有先於 kick"; FAIL=$((FAIL + 1)); }
equals "lib 內容" "$(cat "$DIR/bin/lib-common.sh")" "echo lib"
equals "kick 跑得起來（lib 在旁邊）" "$(bash "$DIR/bin/a-kick.sh" | tail -1)" "a-v2"
check "沒標 shared 的新檔仍 not-installed" "not-installed bin/new-one.sh" "$OUT"
gone "new-one 沒被裝" "$DIR/bin/new-one.sh"
equals "再跑一次 exit 0" "$(run)" "0"
check "lib 已在就 unchanged" "unchanged bin/lib-common.sh" "$OUT"
teardown

echo "ops-install_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
