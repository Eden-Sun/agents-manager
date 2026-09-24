#!/bin/bash
# agm.py 讀 lease_token 的那段（issue #477）。
#
# lease_token 是「只在 acquire 回應出現一次、任何 API 都查不到」的一次性憑證：拿到就能收掉別人
# 正在換 binary 的窗口。所以它不能進 argv（同一個 uid 的行程用 `ps` 就看得到），只能走 600 的檔
# 或 stdin。這支直接呼叫 `lease_token_of`，驗的是那層取用規則本身，不連任何 daemon。
#
#   bash scripts/agm-lease-token_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
  echo "skip - 沒有 python3"; exit 0
fi
AGM_PY="$HERE/agm.py" python3 - <<'PY'
import importlib.util
import os
import subprocess
import sys
import tempfile
import types

spec = importlib.util.spec_from_file_location("agm_under_test", os.environ["AGM_PY"])
agm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agm)

PASS = 0
FAIL = 0


def ok(desc):
    global PASS
    PASS += 1
    print(f"ok   - {desc}")


def bad(desc, detail):
    global FAIL
    FAIL += 1
    print(f"FAIL - {desc}\n      {detail}")


def args(**kw):
    return types.SimpleNamespace(lease_token=kw.get("inline"), lease_token_file=kw.get("path"))


def expect_error(desc, ns, needle, stdin=None):
    try:
        got = agm.lease_token_of(ns)
    except agm.AgmError as e:
        if needle in e.message and e.exit_code == 2:
            ok(desc)
        else:
            bad(desc, f"訊息或結束碼不對：exit={e.exit_code} message={e.message}")
    else:
        bad(desc, f"沒有拒絕，回了 {got!r}")


tmp = tempfile.mkdtemp()
good = os.path.join(tmp, "tok600")
with open(good, "w") as fh:
    fh.write("tok-abc123\n")
os.chmod(good, 0o600)

# 1. 600 的檔：讀得到，尾巴的換行要去掉。
got = agm.lease_token_of(args(path=good))
ok("600 的檔讀得到 token") if got == "tok-abc123" else bad("600 的檔讀得到 token", repr(got))

# 2. group／other 讀得到就當它已經外洩，拒絕而不是照用。
loose = os.path.join(tmp, "tok644")
with open(loose, "w") as fh:
    fh.write("tok-abc123")
os.chmod(loose, 0o644)
expect_error("權限太鬆就拒絕", args(path=loose), "權限是 644")

# 3. symlink：os.stat 會跟過去驗到目標的權限，驗過的就不是實際讀的那個檔（#89 的 TOCTOU 形狀）。
link = os.path.join(tmp, "link")
os.symlink(good, link)
expect_error("不吃 symlink", args(path=link), "讀不到 lease token 檔")

# 4. 空檔：不要靜靜地送出空 token（daemon 會當成「沒帶」而不是「帶錯」）。
empty = os.path.join(tmp, "empty")
open(empty, "w").close()
os.chmod(empty, 0o600)
expect_error("空檔要講清楚", args(path=empty), "是空的")

# 4b. 兩行：`.strip()` 只去頭尾，中間的換行會被當成 token 的一部分送出去，daemon 只回「token 不符」。
two = os.path.join(tmp, "two")
with open(two, "w") as fh:
    fh.write("FAKE-TOKEN-AAA\nFAKE-TOKEN-BBB\n")
os.chmod(two, 0o600)
expect_error("多行要講清楚是幾行", args(path=two), "有 2 行")

# 4c. 尾巴多一個空行不算多行（只是 printf 的習慣），照樣讀得到。
trailing = os.path.join(tmp, "trailing")
with open(trailing, "w") as fh:
    fh.write("tok-abc123\n\n")
os.chmod(trailing, 0o600)
got = agm.lease_token_of(args(path=trailing))
ok("尾巴多空行照樣讀得到") if got == "tok-abc123" else bad("尾巴多空行照樣讀得到", repr(got))

# 5. 檔案不存在。
expect_error("檔不存在要講清楚", args(path=os.path.join(tmp, "nope")), "讀不到 lease token 檔")

# 6. 兩個旗標一起給：不要猜哪個算數。
expect_error("兩個旗標只能給一個", args(path=good, inline="tok-x"), "只能給一個")

# 7. 沒給：空字串（呼叫端據此決定不帶這個欄位，舊 daemon 相容）。
got = agm.lease_token_of(args())
ok("沒給就是空字串") if got == "" else bad("沒給就是空字串", repr(got))

# 8. `-` 走 stdin：跑一個子行程才餵得到真的 stdin。
code = (
    "import importlib.util,os,types;"
    "spec=importlib.util.spec_from_file_location('a',os.environ['AGM_PY']);"
    "m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);"
    "print(m.lease_token_of(types.SimpleNamespace(lease_token='-',lease_token_file=None)))"
)
out = subprocess.run([sys.executable, "-c", code], input="tok-from-stdin\n",
                     capture_output=True, text=True, env=dict(os.environ))
ok("- 從 stdin 讀") if out.stdout.strip() == "tok-from-stdin" else bad("- 從 stdin 讀", out.stdout + out.stderr)

# 9. 檔案權限的檢查是 open 之後才 fstat 的：程式碼層面確認，不要退回先 stat 再 open。
src = open(os.environ["AGM_PY"], encoding="utf-8").read()
body = src[src.index("def lease_token_of"):src.index("def cmd_lease")]
if "os.fstat(" in body and "O_NOFOLLOW" in body and "os.stat(" not in body:
    ok("先 open 再 fstat，而且不跟隨 symlink")
else:
    bad("先 open 再 fstat，而且不跟隨 symlink", "lease_token_of 裡還有 os.stat／少了 O_NOFOLLOW")

for f in (good, loose, empty, link, two, trailing):
    os.remove(f)
os.rmdir(tmp)
print(f"{PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)
PY
