#!/usr/bin/env python3
"""#266 取樣：把 GitHub Actions 的紅 run 抓成本機 dump（不呼叫 TypeSafe，只讀 GitHub）。

    python3 collect.py <work-dir>        # 預設 ./_work

產出 <work-dir>/runs.json、jobs/<run>.json、logs/<job>.txt，
給 build_cases.py 用。重跑會沿用已抓到的檔，只補缺的。
"""
import concurrent.futures as cf
import json
import os
import subprocess
import sys

REPO = "Eden-Sun/agents-manager"
GH = os.environ.get("GH_BIN", "gh")
PAGES = 4  # 每頁 100 筆


def gh(*args, allow_escape=False):
    cmd = [GH, "api"] + (["--allow-escape-sequences"] if allow_escape else []) + list(args)
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.stdout, r.stderr


def main(work):
    os.makedirs(f"{work}/jobs", exist_ok=True)
    os.makedirs(f"{work}/logs", exist_ok=True)
    runs = []
    for p in range(1, PAGES + 1):
        out, err = gh(f"repos/{REPO}/actions/runs?per_page=100&page={p}")
        if not out:
            sys.exit("runs 抓不到：" + err[:200])
        for r in json.loads(out)["workflow_runs"]:
            runs.append({"id": r["id"], "name": r["name"], "head_branch": r["head_branch"],
                         "head_sha": r["head_sha"], "conclusion": r["conclusion"], "status": r["status"],
                         "created_at": r["created_at"], "run_attempt": r["run_attempt"],
                         "event": r["event"], "title": r["display_title"]})
    runs.sort(key=lambda r: r["created_at"])
    json.dump(runs, open(f"{work}/runs.json", "w"), ensure_ascii=False)
    targets = [r for r in runs if r["conclusion"] == "failure" or r["run_attempt"] > 1]
    print(f"{len(runs)} runs，{len(targets)} 個要看 job", file=sys.stderr)

    def jobs_of(r):
        p = f"{work}/jobs/{r['id']}.json"
        if os.path.exists(p) and os.path.getsize(p) > 10:
            return
        out, _ = gh(f"repos/{REPO}/actions/runs/{r['id']}/jobs?per_page=50&filter=all")
        open(p, "w").write(out)

    with cf.ThreadPoolExecutor(6) as ex:
        list(ex.map(jobs_of, targets))

    fails = []
    for r in targets:
        d = json.load(open(f"{work}/jobs/{r['id']}.json"))
        for j in d.get("jobs", []):
            if j["conclusion"] != "failure":
                continue
            fails.append({"job_id": j["id"], "run": r["id"], "attempt": j["run_attempt"], "name": j["name"],
                          "steps": [s["name"] for s in j.get("steps", []) if s["conclusion"] == "failure"],
                          "sha": r["head_sha"], "title": r["title"], "created": r["created_at"],
                          "branch": r["head_branch"], "event": r["event"]})
    json.dump(fails, open(f"{work}/failjobs.json", "w"), ensure_ascii=False)

    def log_of(j):
        p = f"{work}/logs/{j['job_id']}.txt"
        if os.path.exists(p) and os.path.getsize(p) > 500:
            return
        # 不加 --allow-escape-sequences 的話 gh 會拒印整份 log。
        out, err = gh(f"repos/{REPO}/actions/jobs/{j['job_id']}/logs", allow_escape=True)
        open(p, "w").write(out or ("ERR:" + err[:300]))

    with cf.ThreadPoolExecutor(6) as ex:
        list(ex.map(log_of, fails))
    print(f"{len(fails)} 個紅 job、log 都在 {work}/logs", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "_work")
