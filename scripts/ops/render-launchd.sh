#!/bin/sh
set -eu
[ $# -ge 1 ] || { echo "用法：render-launchd.sh <樣板.plist> [<輸出檔>]" >&2; exit 2; }
src=$1
xml_home=$(printf '%s' "$HOME" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g')
sed_home=$(printf '%s' "$xml_home" | sed -e 's/[|\\&]/\\&/g')
if [ $# -ge 2 ]; then
  sed "s|@HOME@|$sed_home|g" "$src" > "$2"
else
  sed "s|@HOME@|$sed_home|g" "$src"
fi
