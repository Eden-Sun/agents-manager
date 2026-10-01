#!/bin/bash
# ops-install.sh 的隔離測試：假 repo（真的 git，帶 install-manifest.tsv）＋暫存的假 AGM 目錄，不碰正式的 AGM bin。
# 釘住：只動「已經裝了、內容跟 repo 不同」的清單內檔案；先備份、原子替換；裝完自檢（bash -n／py 語法），
# 沒過就還原那一支、其他照裝；新檔、排程 unit、清單外的檔一律不動；--dry-run 什麼都不寫。
#
#   bash scripts/ops/ops-install_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/ops-install.sh"
GITBIN=$(command -v git)
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
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
  # 安裝端：照 v1 裝好（new-one、mac-only 沒裝；x.timer 裝的是跟 repo 不同的舊版）
  install -m 755 "$REPO/scripts/ops/a-kick.sh" "$DIR/bin/a-kick.sh"
  install -m 755 "$REPO/scripts/ops/b-tool.py" "$DIR/bin/b-tool.py"
  install -m 644 "$REPO/scripts/ops/c-task.md" "$DIR/c-task.md"
  install -m 755 "$REPO/scripts/ops/linux-only.sh" "$DIR/bin/linux-only.sh"
  mkdir -p "$DIR/systemd"; printf '[Timer]\nOnCalendar=hourly\n' > "$DIR/systemd/x.timer"
  printf 'unversioned\n' > "$DIR/bin/not-in-manifest.sh"
}
teardown() { rm -rf "$ROOT"; }
bump() { # bump <檔> <內容>：repo 裡改一支並 commit
  printf '%s' "$2" > "$REPO/scripts/ops/$1"
  "$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m "bump $1"
}
run() { bash "$SCRIPT" --repo "$REPO" --ref HEAD --dir "$DIR" --platform linux "$@" >"$OUT" 2>&1; echo $?; }

# 1. 沒有變動：什麼都不寫，changes=0。
setup
equals "一致時 exit 0" "$(run)" "0"
check "changes=0" "changes=0" "$OUT"
gone "沒有備份目錄" "$DIR/ops-install-backups"
teardown

# 2. 有變動：先備份舊檔、換成新的、保留權限；清單外／新檔／排程 unit／別的平台不動。
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

# 3. --dry-run：列出會裝什麼，什麼都不寫。
setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
equals "dry-run exit 0" "$(run --dry-run)" "0"
check "列出會裝" "would-install bin/a-kick.sh" "$OUT"
check "changes=1" "changes=1" "$OUT"
equals "安裝端沒動" "$(sed -n 2p "$DIR/bin/a-kick.sh")" "echo a-v1"
gone "沒有備份目錄" "$DIR/ops-install-backups"
gone "沒有 last 記錄" "$DIR/ops-install.last"
teardown

# 4. 自檢失敗：語法壞掉的新版要還原成舊檔、exit 1、點名；其他支照裝。
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

# 5. python 語法壞掉也要擋，而且自檢不能在安裝目錄留 __pycache__。
setup
bump b-tool.py $'def (:\n'
equals "py 語法壞 exit 1" "$(run)" "1"
check "點名 py" "failed bin/b-tool.py" "$OUT"
equals "py 還原" "$(cat "$DIR/bin/b-tool.py")" 'print("b-v1")'
gone "沒有 __pycache__" "$DIR/bin/__pycache__"
teardown

# 6. 來源在這個 ref 讀不到（清單寫了、repo 裡沒有）：報 failed、不動安裝端。
setup
printf 'scripts/ops/ghost.sh  bin/ghost.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m ghost
printf '#!/bin/bash\n' > "$DIR/bin/ghost.sh"
equals "來源缺 exit 1" "$(run)" "1"
check "點名" "failed bin/ghost.sh" "$OUT"
teardown

# 7. 用法錯誤：缺必要參數 exit 2。
setup
equals "沒帶 --dir exit 2" "$(bash "$SCRIPT" --repo "$REPO" >"$OUT" 2>&1; echo $?)" "2"
teardown

# 8. drift：安裝端的檔不是 repo 任何一版（有人手改過）＝不覆蓋，報 drifted，其他支照裝；--force 才換（照樣先備份）。
#    判斷跟 `agm ops-sync --check` 的 drift 同一條（安裝檔的 blob 不在 `git log <ref> -- <來源>` 的任何一版裡）。
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
B=$(ls -d "$DIR"/ops-install-backups/*/ | tail -1)
equals "手改的版本在備份裡" "$(sed -n 2p "${B}bin/a-kick.sh")" "echo a-hand-edited"
teardown

# 9. 自檢在**暫存檔**上做、過了才換：自檢跑的那一刻，安裝位置上還是舊檔（不能讓 cron／launchd 撞到沒驗過的新版）。
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

# 11. 安裝位置是 symlink（使用者把它連到別處）：不換、不把連結吃掉，報 skipped。
setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
cp "$DIR/bin/a-kick.sh" "$ROOT/elsewhere.sh"; rm "$DIR/bin/a-kick.sh"; ln -s "$ROOT/elsewhere.sh" "$DIR/bin/a-kick.sh"
equals "symlink：exit 0" "$(run)" "0"
check "報 skipped" "skipped bin/a-kick.sh" "$OUT"
equals "連結還在" "$([ -L "$DIR/bin/a-kick.sh" ] && echo link)" "link"
equals "連結指到的檔沒動" "$(sed -n 2p "$ROOT/elsewhere.sh")" "echo a-v1"
teardown

# 12. 對照表的安裝位置跑出 AGM 目錄（絕對路徑、`..`）：不寫，報 failed。
setup
printf '#!/bin/bash\necho outside\n' > "$ROOT/escape.sh"
printf 'scripts/ops/esc.sh  ../escape.sh\n' >> "$REPO/scripts/ops/install-manifest.tsv"
printf '#!/bin/bash\necho esc-v1\n' > "$REPO/scripts/ops/esc.sh"
"$GITBIN" -C "$REPO" add -A; "$GITBIN" -C "$REPO" commit -q -m esc
equals "路徑跑出去 exit 1" "$(run)" "1"
check "點名" "failed ../escape.sh" "$OUT"
equals "目錄外的檔沒動" "$(sed -n 2p "$ROOT/escape.sh")" "echo outside"
teardown

# 13. 同一秒內連裝兩次（兩個 ref）：第二次的備份不能蓋掉第一次留下的原始檔。
setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
equals "第一次裝 exit 0" "$(run)" "0"
bump a-kick.sh $'#!/bin/bash\necho a-v3\n'
equals "第二次裝 exit 0" "$(run)" "0"
equals "兩份備份都在（v1 沒被 v2 蓋掉）" "$(cat "$DIR"/ops-install-backups/*/bin/a-kick.sh | grep -c 'echo a-v1\|echo a-v2')" "2"
teardown

# 10. 同時只能有一個在裝：鎖被活著的行程握著就不動任何檔（exit 3）；握鎖的行程死了（殘留的鎖）就接手。
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

# 14. kick 自己更新自己：正在跑的那支腳本被換掉（`mv` 換 inode，bash 手上還是舊檔），後面的行照常跑完，不會讀到新檔的中段。
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

# 15. 被 SIGKILL 的上一次留下的暫存檔（<安裝位置>.new.<pid>，pid 已不在）：下次執行清掉；pid 還活著的、名字不是 .new.<數字> 的、清單外的不碰。
setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
sleep 30 & LIVE=$!
DEAD=$(sh -c 'echo $$')   # 這個 shell 已經結束，pid 不在了
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

# 15b. --dry-run 什麼都不清。
setup
bump a-kick.sh $'#!/bin/bash\necho a-v2\n'
DEAD=$(sh -c 'echo $$')
printf 'half' > "$DIR/bin/a-kick.sh.new.$DEAD"
run --dry-run >/dev/null
equals "dry-run 不清暫存檔" "$(cat "$DIR/bin/a-kick.sh.new.$DEAD")" "half"
teardown

echo "ops-install_test: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
