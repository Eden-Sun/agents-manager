#!/usr/bin/env bash
# 每一個 Rust 測試檔都要有人把它編進去：crate 拆分時 6 個測試檔（150 條）與 5 個 harness 檔沒有掛回任何模組，
# 從 2026-10-07 起沒編過，整樹 CI 照樣綠（#1334、#1335、#1336）。這支只做純文字比對（git 追蹤的檔），不編譯。
# 真樹檢查與假樹自我檢查都在這支裡：check.sh ops 的 scripts/*_test.sh 迴圈會撈到它。
#
#   scripts/rust_test_mount_test.sh                      # 檢查 repo（或 AM_TEST_MOUNT_ROOT 指的樹）＋自我檢查
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"

# 已知還沒掛回去的（各有票在修）：修好一個就從這裡刪一行；清單只能變短。
KNOWN=(
    crates/am-base/src/release_triage/tests.rs
    crates/am-base/src/release_triage/tests_publish.rs
    crates/am-base/src/host_baseline/tests.rs
    crates/am-base/src/rewind/tests.rs
    crates/am-base/src/agy_remote/tests.rs
    crates/am-base/src/prompt_suggestion/tests.rs
)
# daemon/src/runners/am_base_tests/ 底下還沒在 am_base_tests.rs 宣告的 harness 檔（#1334／#1335 修好一個就刪一行）。
HARNESS_DIR=daemon/src/runners/am_base_tests
HARNESS_ROOT=daemon/src/runners/am_base_tests.rs
KNOWN_HARNESS=(
    daemon/src/runners/am_base_tests/agy_remote.rs
    daemon/src/runners/am_base_tests/host_baseline.rs
    daemon/src/runners/am_base_tests/prompt_suggestion.rs
    daemon/src/runners/am_base_tests/release_triage.rs
    daemon/src/runners/am_base_tests/rewind.rs
)

# 候選：daemon/src 與 crates/*/src 底下檔名是 tests.rs、tests_*.rs、*_tests.rs、*.daemon-tests-*.rs 的檔。
is_candidate() {
    case "$1" in
        tests.rs | tests_*.rs | *_tests.rs | *.daemon-tests-*.rs) return 0 ;;
    esac
    return 1
}

in_list() {  # <needle> <item>…
    local needle="$1" item
    shift
    for item in "$@"; do
        [ "$item" = "$needle" ] && return 0
    done
    return 1
}

# check_tree <root>：印 FAIL 行到 stdout，回傳失敗數。
check_tree() {
    local root="$1" fail=0 tracked attrs f base mounted
    tracked="$(git -C "$root" ls-files -- '*.rs')"
    # 所有 #[path = "…"] 的值（去掉開頭的 ./ 與 ../）。
    attrs="$(git -C "$root" grep -h -E '#\[path[[:space:]]*=' -- '*.rs' 2>/dev/null |
        sed -n -E 's/.*#\[path[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' |
        sed -E 's#^(\.\./|\./)+##' || true)"

    # 有 #[path] 的值以 /<值> 結尾、或等於 <值>，就算掛上（值通常是相對於宣告檔的路徑，所以用結尾比對）。
    mounted_by_path() {  # <repo 內的路徑>
        local v
        while IFS= read -r v; do
            [ -n "$v" ] || continue
            case "$1" in "$v" | */"$v") return 0 ;; esac
        done <<<"$attrs"
        return 1
    }
    # 同名模組宣告：<dir>/<名字>.rs 的同層 mod.rs／lib.rs／main.rs，或上一層的 <目錄名>.rs，含有 mod <名字>;。
    declared_by_mod() {  # <repo 內的路徑>
        local stem dir dname d
        stem="$(basename "$1" .rs)"
        dir="$(dirname "$1")"
        dname="$(basename "$dir")"
        for d in "$root/$dir/mod.rs" "$root/$dir/lib.rs" "$root/$dir/main.rs" "$root/$(dirname "$dir")/$dname.rs"; do
            [ -f "$d" ] || continue
            if grep -qE "^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${stem}[[:space:]]*;" "$d"; then
                return 0
            fi
        done
        return 1
    }
    is_mounted() {  # <repo 內的路徑>
        mounted_by_path "$1" || declared_by_mod "$1"
    }

    # 1) 候選沒人掛、又不在 KNOWN → 失敗；KNOWN 裡的候選已經掛上或檔案不在 → 失敗（棘輪）。
    while IFS= read -r f; do
        case "$f" in daemon/src/* | crates/*/src/*) ;; *) continue ;; esac
        is_candidate "$(basename "$f")" || continue
        if is_mounted "$f"; then
            mounted=1
        else
            mounted=0
        fi
        if in_list "$f" "${KNOWN[@]}"; then
            continue
        fi
        if [ "$mounted" = 0 ]; then
            printf 'FAIL - %s 沒有被任何 mod／#[path] 掛進去（裡面的測試不會跑）\n' "$f"
            fail=$((fail + 1))
        fi
    done <<<"$tracked"

    for f in "${KNOWN[@]}"; do
        if ! in_list "$f" $tracked; then
            printf 'FAIL - %s 檔案不在了，從 KNOWN 刪掉\n' "$f"
            fail=$((fail + 1))
        elif is_mounted "$f"; then
            printf 'FAIL - %s 已經有人掛進去了，從 KNOWN 刪掉\n' "$f"
            fail=$((fail + 1))
        fi
    done

    # 2) harness：am_base_tests/ 底下每一個 .rs 都要在 am_base_tests.rs 有 #[path = "am_base_tests/<檔名>"]。
    while IFS= read -r f; do
        case "$f" in "$HARNESS_DIR"/*.rs) ;; *) continue ;; esac
        base="$(basename "$f")"
        mounted=0
        if [ -f "$root/$HARNESS_ROOT" ] &&
            grep -qE "#\[path[[:space:]]*=[[:space:]]*\"am_base_tests/${base}\"\]" "$root/$HARNESS_ROOT"; then
            mounted=1
        fi
        if in_list "$f" "${KNOWN_HARNESS[@]}"; then
            continue
        fi
        if [ "$mounted" = 0 ]; then
            printf 'FAIL - %s 沒有在 %s 宣告（裡面的測試不會跑）\n' "$f" "$HARNESS_ROOT"
            fail=$((fail + 1))
        fi
    done <<<"$tracked"

    for f in "${KNOWN_HARNESS[@]}"; do
        base="$(basename "$f")"
        if ! in_list "$f" $tracked; then
            printf 'FAIL - %s 檔案不在了，從 KNOWN_HARNESS 刪掉\n' "$f"
            fail=$((fail + 1))
        elif [ -f "$root/$HARNESS_ROOT" ] &&
            grep -qE "#\[path[[:space:]]*=[[:space:]]*\"am_base_tests/${base}\"\]" "$root/$HARNESS_ROOT"; then
            printf 'FAIL - %s 已經掛上了，從 KNOWN_HARNESS 刪掉\n' "$f"
            fail=$((fail + 1))
        fi
    done

    return "$fail"
}

# ---------------------------------------------------------------- 自我檢查（假的 git 樹，放在 TMPDIR）
SELF_TMP=""
cleanup() {
    [ -z "$SELF_TMP" ] || rm -rf "$SELF_TMP"
}
trap cleanup EXIT

# new_tree <dir>：基線＝KNOWN／KNOWN_HARNESS 的 stub 都在、都沒掛，am_base_tests.rs 沒有宣告。
new_tree() {
    local d="$1" k
    mkdir -p "$d/$HARNESS_DIR"
    git -C "$d" init -q
    for k in "${KNOWN[@]}" "${KNOWN_HARNESS[@]}"; do
        mkdir -p "$d/$(dirname "$k")"
        : >"$d/$k"
    done
    # 真樹的 am_base_tests.rs 是 runners/mod.rs 用 #[path] 掛的；假樹照做，它才不會被當成沒掛。
    printf '#[path = "am_base_tests.rs"]\npub(crate) mod am_base_tests;\n' >"$(dirname "$d/$HARNESS_ROOT")/mod.rs"
    : >"$d/$HARNESS_ROOT"
    git -C "$d" add -A
}

st_fail=0
# expect_pass <名稱> <dir>；expect_fail <名稱> <dir> <必須出現的字串>
expect_pass() {
    local out rc=0
    out="$(check_tree "$2" 2>&1)" || rc=$?
    if [ "$rc" != 0 ]; then
        printf 'self-test FAIL（%s）：應該放行，卻失敗\n%s\n' "$1" "$out" >&2
        st_fail=1
    fi
}
expect_fail() {
    local out rc=0
    out="$(check_tree "$2" 2>&1)" || rc=$?
    if [ "$rc" = 0 ] || ! grep -qF -- "$3" <<<"$out"; then
        printf 'self-test FAIL（%s）：應該失敗並點名 %s，實際 rc=%s\n%s\n' "$1" "$3" "$rc" "$out" >&2
        st_fail=1
    fi
}

self_test() {
    local base_root
    SELF_TMP="$(mktemp -d "${TMPDIR:-/tmp}/rust-test-mount.XXXXXX")"

    base_root="$SELF_TMP/base"
    new_tree "$base_root"
    expect_pass "基線：KNOWN 與 harness 都未掛" "$base_root"

    # 新的候選檔沒人掛 → 點名。
    local t="$SELF_TMP/unmounted"
    new_tree "$t"
    mkdir -p "$t/crates/x/src/foo"
    : >"$t/crates/x/src/foo/tests.rs"
    printf 'pub fn a() {}\n' >"$t/crates/x/src/foo.rs"
    git -C "$t" add -A
    expect_fail "候選沒人掛" "$t" "crates/x/src/foo/tests.rs"
    # 同名模組宣告（foo.rs 裡 mod tests;）→ 放行；foo.rs 本身要從 crate 根掛上（#1423：宣告檔沒掛就不算）。
    printf '#[cfg(test)]\nmod tests;\n' >>"$t/crates/x/src/foo.rs"
    printf 'mod foo;\n' >"$t/crates/x/src/lib.rs"
    git -C "$t" add -A
    expect_pass "候選由同名模組宣告" "$t"

    # #[path] 掛上（值含目錄）→ 放行；bare 的 #[path] 值同樣只看結尾。
    t="$SELF_TMP/pathed"
    new_tree "$t"
    mkdir -p "$t/crates/x/src/bar"
    : >"$t/crates/x/src/bar/tests_more.rs"
    printf '#[path = "bar/tests_more.rs"]\nmod tests_more;\n' >"$t/crates/x/src/lib.rs"
    git -C "$t" add -A
    expect_pass "候選由 #[path] 掛上" "$t"

    # #1423 (a)：dead.rs 有 mod tests; 但 dead.rs 自己沒有從 crate 根被掛上 → 點名 dead.rs（不能因為「同層有人宣告」就放行）。
    t="$SELF_TMP/dead-parent"
    new_tree "$t"
    mkdir -p "$t/crates/x/src/dead"
    printf 'pub fn a() {}\n' >"$t/crates/x/src/lib.rs"
    printf '#[cfg(test)]\nmod tests;\n' >"$t/crates/x/src/dead.rs"
    : >"$t/crates/x/src/dead/tests.rs"
    git -C "$t" add -A
    expect_fail "宣告檔本身沒掛" "$t" "crates/x/src/dead.rs"

    # #1423 (b)：a.rs 的 #[path = "tests.rs"] 解析成 crates/x/src/tests.rs（依宣告檔所在目錄）；b/tests.rs 不能因為結尾同名就算掛上。
    t="$SELF_TMP/path-suffix"
    new_tree "$t"
    mkdir -p "$t/crates/x/src/b"
    printf 'mod a;\n' >"$t/crates/x/src/lib.rs"
    printf '#[path = "tests.rs"]\nmod t;\n' >"$t/crates/x/src/a.rs"
    : >"$t/crates/x/src/tests.rs"
    : >"$t/crates/x/src/b/tests.rs"
    git -C "$t" add -A
    expect_fail "#[path] 只做結尾比對" "$t" "crates/x/src/b/tests.rs"

    # KNOWN 的檔已經被掛上 → 點名要從 KNOWN 刪掉（只掛 rewind，不能連帶影響 release_triage 的 tests.rs）。
    t="$SELF_TMP/known-mounted"
    new_tree "$t"
    printf '#[path = "rewind/tests.rs"]\nmod rewind_tests;\n' >"$t/crates/am-base/src/lib.rs"
    git -C "$t" add -A
    expect_fail "KNOWN 已掛上" "$t" "crates/am-base/src/rewind/tests.rs"

    # KNOWN 的檔不在了 → 點名。
    t="$SELF_TMP/known-gone"
    new_tree "$t"
    rm -f "$t/${KNOWN[0]}"
    git -C "$t" add -A
    expect_fail "KNOWN 檔案不在了" "$t" "${KNOWN[0]}"

    # harness：沒宣告的新檔 → 點名；已宣告的 KNOWN_HARNESS → 點名。
    t="$SELF_TMP/harness-new"
    new_tree "$t"
    : >"$t/$HARNESS_DIR/newone.rs"
    git -C "$t" add -A
    expect_fail "harness 沒宣告" "$t" "$HARNESS_DIR/newone.rs"

    t="$SELF_TMP/harness-mounted"
    new_tree "$t"
    printf '#[path = "am_base_tests/rewind.rs"]\nmod rewind;\n' >"$t/$HARNESS_ROOT"
    git -C "$t" add -A
    expect_fail "KNOWN_HARNESS 已掛上" "$t" "$HARNESS_DIR/rewind.rs"

    if [ "$st_fail" != 0 ]; then
        return 1
    fi
}

main() {
    local root="${AM_TEST_MOUNT_ROOT:-$REPO}" fail=0
    check_tree "$root" || fail=$?
    if [ "$fail" != 0 ]; then
        exit 1
    fi
    self_test || exit 1
    echo "rust test mount: OK（已知未掛 ${#KNOWN[@]} 個＋harness ${#KNOWN_HARNESS[@]} 個）"
}

main "$@"
