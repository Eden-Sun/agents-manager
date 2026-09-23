#!/usr/bin/env python3
"""#266：把 collect.py 的 dump ＋ labels.json 變成兩組題目（choice 一題 vs. noul 三題）。

    python3 build_cases.py <work-dir> <out-dir>

同一批 state，兩個檔各問一種，才能比「一次五選一」跟「三個獨立機率」哪個準。
state 只放判斷當下拿得到的東西：job／step／失敗測試名／log 摘錄／commit 標題與改到的路徑／
同一個 job 與同一條測試在這次之前的紅綠序列。不含任何未來資訊。
"""
import json
import os
import re
import subprocess
import sys

ESC = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
TS = re.compile(r"^20\d\d-\d\d-\d\dT[\d:.]+Z ", re.M)
DROP = re.compile(r"^(\[command\]|Post job cleanup|Copying |Temporarily overriding|Adding repository"
                  r"|Cleaning up orphan|##\[group\]|##\[endgroup\]|Node 20 is being|##\[warning\]Node"
                  r"|\s*Compiling |\s*Downloaded |\s*Updating crates|warning: unused)")
# log 已經被 GitHub 遮過一次，這裡再擋一次明顯的憑據形狀。
SECRET = re.compile(r"(gh[pusor]_[A-Za-z0-9]{10,}|Bearer [A-Za-z0-9._\-]{10,}|x-access-token:[^@\s]+"
                    r"|AUTHORIZATION: [^\s]+|[A-Za-z0-9+/]{60,}={0,2})")
LIMIT = 3000


def excerpt(path):
    t = TS.sub("", ESC.sub("", open(path, errors="replace").read()))
    lines = [l for l in t.splitlines() if not DROP.match(l)]
    anchors = [i for i, l in enumerate(lines)
               if re.match(r"^(failures:|ERROR: |FAIL: |error(\[|:)|thread '.* panicked)", l)]
    i = anchors[0] if anchors else max(0, len(lines) - 60)
    head = "\n".join(lines[max(0, i - 5):])
    tail = [l for l in lines[-40:]
            if re.match(r"^(test result:|Ran \d+ tests|FAILED |OK$|error: test failed|##\[error\])", l)]
    tail = "\n".join(tail[-6:])
    if len(head) > LIMIT - len(tail) - 20:
        head = head[:LIMIT - len(tail) - 20] + "\n...[裁切]...\n"
    return SECRET.sub("[REDACTED]", head + "\n" + tail)


def failed_tests(path, job):
    t = TS.sub("", ESC.sub("", open(path, errors="replace").read()))
    if job == "daemon":
        n = sorted(set(re.findall(r"^(\S+) \.\.\. FAILED", t, re.M)))
        return n or sorted(set(re.findall(r"----\s+(\S+)\s+stdout\s+----", t)))
    n = sorted(set(re.findall(r"^(?:ERROR|FAIL): (\S+) ", t, re.M)))
    return n or ["scripts/ob_test.py"]


def history(work):
    """回傳 {job_id: {"job_last_12": "GGRRG…", "test_prior_consecutive": {test: n}}}，只看這次之前。"""
    runs = json.load(open(f"{work}/runs.json"))
    runs.sort(key=lambda r: r["created_at"])
    fails = {(j["run"], j["name"]): j for j in json.load(open(f"{work}/failjobs.json"))}
    out, seq, streak = {}, {"ob": [], "daemon": []}, {"ob": {}, "daemon": {}}
    for r in runs:
        if r["conclusion"] not in ("success", "failure"):
            continue
        for name in ("ob", "daemon"):
            j = fails.get((r["id"], name))
            if not j:
                seq[name].append("G")
                streak[name] = {}
                continue
            tests = failed_tests(f"{work}/logs/{j['job_id']}.txt", name)
            out[str(j["job_id"])] = {"job_last_12": "".join(seq[name][-12:]),
                                     "test_prior_consecutive": {t: streak[name].get(t, 0) for t in tests},
                                     "failed_tests": tests}
            seq[name].append("R")
            streak[name] = {t: streak[name].get(t, 0) + 1 for t in tests}
    return out



CLASSES = {
    "real_regression": {
        "what": "The source code is genuinely broken: re-running this same commit would fail again, and only editing code in the repository can make it pass.",
        "not_for": "a failure that only shows up on this particular machine while the code is fine elsewhere"},
    "runner_environment": {
        "what": "The code works on a developer machine; it fails here because the CI runner is a different machine — a program or app the test expects to be running locally is absent, the OS resolves paths or signals differently, tool versions differ. On this runner it fails every time.",
        "not_for": "a failure that comes and goes on the same runner"},
    "flaky_timing": {
        "what": "The same commit passes sometimes and fails sometimes: a race, a wait that is too short, or parallel tests interfering. Re-running the identical commit has a good chance of going green.",
        "not_for": "a failure that has been red on every run for a long time"},
    "infra": {
        "what": "The failure happened outside the repository's own code: checkout, dependency download, toolchain install, the network, or a third-party service being down."},
    "unclear": {
        "what": "The evidence given is not enough to pick one of the others."},
}

PREAMBLE = ("`log` is the failing part of a GitHub Actions job on a macOS runner for a Rust + TypeScript repository. "
            "`history.job_last_12` is that job's pass/fail on the 12 pushes before this one, oldest first, G=green R=red. "
            "`history.test_prior_consecutive` is, per failing test, how many runs in a row it had already been failing "
            "immediately before this one (0 = the previous run was green or that test was not failing).")

Q_CHOICE = {"failure_class": {
    "type": "choice",
    "instructions": PREAMBLE + " Why is this job red?",
    "criteria": CLASSES}}

Q_NOUL = {
    "rerun_would_pass": {
        "type": "noul",
        "instructions": {"question": "If this exact commit were re-run on a fresh runner with nothing changed, would it probably go green?",
                         "focus": PREAMBLE},
        "criteria": {"true": "the failure comes and goes on identical input",
                     "false": "the same input reliably produces this failure"}},
    "machine_specific": {
        "type": "noul",
        "instructions": {"question": "Does this failure come from the CI runner being a different machine from the developer's — a program or app the test expects to be running locally is absent, paths or signals resolve differently, tool versions differ — rather than from the change in this commit?",
                         "focus": PREAMBLE},
        "criteria": {"true": "the same code passes on a developer machine and fails here because of the machine",
                     "false": "the failure would reproduce anywhere"}},
    "introduced_by_this_commit": {
        "type": "noul",
        "instructions": {"question": "Is this failure this commit's own doing, rather than a red the branch was already carrying from an earlier commit?",
                         "focus": PREAMBLE + " Many people push to this branch, so a commit often inherits a red it had nothing to do with. `commit_subject` and `changed_paths` describe this commit."},
        "criteria": {"true": "this commit's change is what broke it",
                     "false": "it was already failing, or something else broke it"}},
}


def main(work, out):
    labels = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "labels.json")))["cases"]
    hist = history(work)
    fails = {str(j["job_id"]): j for j in json.load(open(f"{work}/failjobs.json"))}
    os.makedirs(out, exist_ok=True)
    fc = open(f"{out}/cases_choice.jsonl", "w")
    fn = open(f"{out}/cases_noul.jsonl", "w")
    n = 0
    for jid, lab in labels.items():
        j, h = fails[jid], hist[jid]
        state = {
            "job": j["name"],
            "runner": "macos-latest",
            "failed_step": j["steps"][0] if j["steps"] else "",
            "branch": j["branch"],
            "commit_subject": j["title"],
            "changed_paths": subprocess.run(
                ["/usr/bin/git", "show", "--name-only", "--format=", j["sha"]],
                capture_output=True, text=True).stdout.split()[:25],
            "failed_tests": h["failed_tests"],
            "history": {"job_last_12": h["job_last_12"],
                        "test_prior_consecutive": h["test_prior_consecutive"]},
            "log": excerpt(f"{work}/logs/{jid}.txt"),
        }
        meta = {"sha": j["sha"][:8], "created": j["created"], "job": j["name"],
                "rerun_basis": lab["rerun_basis"], "class": lab["class"],
                "first_occurrence": max(h["test_prior_consecutive"].values(), default=0) == 0}
        cid = f"{j['name']}-{j['sha'][:8]}"
        fc.write(json.dumps({"id": cid, "state": state, "questions": Q_CHOICE,
                             "labels": {"failure_class": lab["class"]}, "meta": meta}, ensure_ascii=False) + "\n")
        nl = {"rerun_would_pass": lab["rerun_pass"], "machine_specific": lab["class"] == "runner_environment"}
        if lab["introduced_by_commit"] is not None:
            nl["introduced_by_this_commit"] = lab["introduced_by_commit"]
        fn.write(json.dumps({"id": cid, "state": state, "questions": Q_NOUL,
                             "labels": nl, "meta": meta}, ensure_ascii=False) + "\n")
        n += 1
    print(f"{n} 題 × 2 組 = {n * 2} 次呼叫", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else os.path.dirname(os.path.abspath(__file__)))
