#!/bin/bash
# cd-trust-gate.py 的隔離測試：每個 case 一個暫存 repo（自己當自己的 origin）與一支假 gh，不連 GitHub。
#
#   bash scripts/ops/cd-trust-gate_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
GATE="$HERE/cd-trust-gate.py"
G=/usr/bin/git
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  REPO="$ROOT/repo"; STATE="$ROOT/state"; ORIGIN="$ROOT/origin.git"
  $G init -q --bare "$ORIGIN"
  $G init -q "$REPO"
  ( cd "$REPO" && $G config user.email owner@example.com && $G config user.name owner \
      && $G remote add origin "$ORIGIN" && mkdir -p daemon scripts/ops .github/workflows \
      && echo v1 > daemon/main.rs && echo ci > .github/workflows/ci.yml && $G add -A && $G commit -qm init ) >/dev/null
  BASE=$($G -C "$REPO" rev-parse HEAD)
  # 假 gh：`pr list` 回 $STUB_PRS；`api …/commits/<sha>/pulls` 回 $ROOT/pulls/<sha>（沒有就 []）；STUB_GH_FAIL=1 全部失敗。
  mkdir -p "$ROOT/pulls"
  cat > "$ROOT/gh" <<'STUB'
#!/bin/bash
[ "${STUB_GH_FAIL:-0}" = 1 ] && { echo "HTTP 502" >&2; exit 1; }
case "$1" in
  pr)  printf '%s' "${STUB_PRS:-[]}" ;;
  api) sha=$(echo "$2" | sed -E 's#.*/commits/([0-9a-f]+)/pulls#\1#'); f="$STUB_ROOT/pulls/$sha"
       if [ -f "$f" ]; then cat "$f"; else printf '[]'; fi ;;
esac
STUB
  chmod +x "$ROOT/gh"
  export STUB_ROOT="$ROOT" STUB_PRS="[]" STUB_GH_FAIL=0
}

commit() { # commit <檔> <內容> [作者 email]
  ( cd "$REPO" && mkdir -p "$(dirname "$1")" && echo "$2" > "$1" && $G add -A \
      && $G -c user.email="${3:-owner@example.com}" -c user.name=x commit -qm "$1" ) >/dev/null
  $G -C "$REPO" rev-parse HEAD
}

gate() { OUT=$(python3 "$GATE" check --repo "$REPO" --from "$BASE" --to "$($G -C "$REPO" rev-parse HEAD)" --state-dir "$STATE" --slug me/proj --gh "$ROOT/gh" 2>&1); RC=$?; }

expect() { # expect <名稱> <exit code> [輸出要含的字]
  if [ "$RC" = "$2" ] && { [ -z "${3:-}" ] || echo "$OUT" | grep -q -- "$3"; }; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1)); echo "FAIL ${1}：rc=${RC}（要 ${2}）out=${OUT}"
  fi
  rm -rf "$ROOT"
}

setup; commit daemon/main.rs v2 >/dev/null; gate
expect "owner 自己推的 commit 放行" 0

setup; gate
expect "沒有新 commit 放行" 0

setup; commit daemon/main.rs v2 >/dev/null; gate; ALLOW=$(grep -v '^#' "$STATE/cd-trust.allow")
RC=$([ "$ALLOW" = "owner@example.com" ] && echo 0 || echo 1); OUT="$ALLOW"
expect "允許名單第一次由已部署的 commit 建立" 0

setup; commit daemon/main.rs v2 mallory@evil.example >/dev/null; gate
expect "名單外的作者擋下" 3 "mallory@evil.example"

# squash merge fork PR：email 偽裝成 owner 也沒用，GitHub 說它屬於 fork PR。
setup; S=$(commit daemon/main.rs v2)
echo '[{"number":7,"user":{"login":"mallory"},"head":{"repo":{"full_name":"mallory/proj"}}}]' > "$ROOT/pulls/$S"; gate
expect "fork PR 的 commit 擋下" 3 "fork PR #7"

setup; S=$(commit daemon/main.rs v2)
echo '[{"number":8,"user":{"login":"mallory"},"head":{"repo":null}}]' > "$ROOT/pulls/$S"; gate
expect "fork 已刪的 PR 一樣擋下" 3 "fork 已刪"

setup; S=$(commit daemon/main.rs v2)
echo '[{"number":9,"user":{"login":"me"},"head":{"repo":{"full_name":"me/proj"}}}]' > "$ROOT/pulls/$S"; gate
expect "本 repo 分支開的 PR 放行" 0

# 有人把 fork PR 的內容 cherry-pick 後直接推 main：GitHub 不會關聯，靠 patch-id。
setup
( cd "$REPO" && $G checkout -q -b fork-work && echo evil > daemon/backdoor.rs && $G add -A \
    && $G -c user.email=mallory@evil.example commit -qm "helpful fix" && $G push -q origin HEAD:refs/pull/12/head \
    && $G checkout -q - && $G cherry-pick fork-work >/dev/null 2>&1 \
    && $G -c user.email=owner@example.com commit -q --amend --reset-author --no-edit ) >/dev/null 2>&1
export STUB_PRS='[{"number":12,"isCrossRepository":true,"author":{"login":"mallory"}}]'; gate
expect "cherry-pick 進來的 fork PR 內容擋下" 3 "fork PR #12"

setup; commit daemon/main.rs v2 >/dev/null
( cd "$REPO" && $G checkout -q -b other "$BASE" && echo unrelated > README && $G add -A && $G commit -qm pr && $G push -q origin HEAD:refs/pull/13/head && $G checkout -q - ) >/dev/null 2>&1
export STUB_PRS='[{"number":13,"isCrossRepository":true,"author":{"login":"someone"}}]'; gate
expect "有 fork PR 開著但內容沒進來：放行" 0

setup; commit scripts/ops/daemon-swap.sh changed >/dev/null; gate
expect "動到換版腳本擋下" 3 "scripts/ops/daemon-swap.sh"

setup; commit .github/workflows/ci.yml changed >/dev/null; gate
expect "CI 定義預設不擋" 0

setup; commit .github/workflows/ci.yml changed >/dev/null; mkdir -p "$STATE"; echo ".github/" > "$STATE/cd-trust.protected"; gate
expect "cd-trust.protected 加的前綴會擋" 3 ".github/workflows/ci.yml"

setup; commit scripts/ops/cd-trust-gate.py neutered >/dev/null; commit scripts/ops/cd-trust-gate.py restored >/dev/null; gate
expect "閘門自己被改過又改回來也擋" 3 "cd-trust-gate.py"

setup; S=$(commit scripts/ops/daemon-swap.sh changed)
python3 "$GATE" approve --state-dir "$STATE" --note "看過了" "$S" >/dev/null; gate
expect "approve 過的 commit 放行" 0

setup; S=$(commit scripts/ops/daemon-swap.sh changed); OUT=$(python3 "$GATE" approve --state-dir "$STATE" --note x "${S:0:12}" 2>&1); RC=$?
expect "approve 不收短 sha" 1 "40"

setup; commit daemon/main.rs v2 >/dev/null; export STUB_GH_FAIL=1; gate
expect "gh 不通＝查不出來，不放行" 4 "查不出來"

setup; OLD=$BASE; ( cd "$REPO" && $G checkout -q --orphan rewritten && echo x > f && $G add -A && $G commit -qm rewritten ) >/dev/null 2>&1; BASE=$OLD; gate
expect "歷史被改寫擋下" 3 "歷史被改寫"

echo "cd-trust-gate: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
