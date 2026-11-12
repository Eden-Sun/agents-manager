#!/bin/sh
# 把 launchd 樣板（scripts/ops/launchd/*.plist）裡的 `@HOME@` 換成這台的 ${HOME}（XML 轉義），
# 寫到第二個參數指定的檔；沒給就寫到 stdout。樣板不寫死任何人的家目錄。
#
#   scripts/ops/render-launchd.sh scripts/ops/launchd/com.agm.outbox-gc.plist ~/Library/LaunchAgents/com.agm.outbox-gc.plist
#
# `agm ops-sync --check` 比對時用同一套代換（agm.py `_render_home`），所以裝出來的檔跟樣板會判成同步。
set -eu
[ $# -ge 1 ] || { echo "用法：render-launchd.sh <樣板.plist> [<輸出檔>]" >&2; exit 2; }
src=$1
# XML 轉義（& 與 <），再轉成 sed 替換字串（\ | & 要跳脫；分隔符用 |）。
xml_home=$(printf '%s' "$HOME" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g')
sed_home=$(printf '%s' "$xml_home" | sed -e 's/[|\\&]/\\&/g')
if [ $# -ge 2 ]; then
  sed "s|@HOME@|$sed_home|g" "$src" > "$2"
else
  sed "s|@HOME@|$sed_home|g" "$src"
fi
