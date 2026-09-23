#!/usr/bin/env python3
"""每則回報的決定性事實：報告裡提到的 commit 在不在 origin/main、CI run 的結論是什麼。

  python3 facts.py cases_zh.jsonl > facts.json

這些事實是 AGM 自己查得到的，Jev 不該取代它們；這裡算出來是為了做矛盾表
（宣稱完成 × commit 不在 main、宣稱 CI 過 × 沒有 run）。gh 查不到就記 unknown。
"""
import json, re, subprocess, sys

SHA = re.compile(r"\b[0-9a-f]{7,40}\b")
RUN = re.compile(r"\b\d{11}\b")
# 太容易誤判的：純數字 id、KB 數字已被 \b[0-9a-f]{7,40}\b 排除（含非 hex 字元才進來）


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True)


def main():
    out = {}
    for line in open(sys.argv[1]):
        if not line.strip():
            continue
        case = json.loads(line)
        text = case["state"]["report"]
        shas, on_main, unknown = [], [], []
        for s in dict.fromkeys(SHA.findall(text)):
            if s.isdigit():  # 純數字，是 pid／大小，不是 sha
                continue
            if git("cat-file", "-e", s + "^{commit}").returncode != 0:
                unknown.append(s)
                continue
            shas.append(s)
            if git("merge-base", "--is-ancestor", s, "origin/main").returncode == 0:
                on_main.append(s)
        runs = {}
        for r in dict.fromkeys(RUN.findall(text)):
            p = subprocess.run(["gh", "run", "view", r, "--json", "conclusion,headSha",
                                "--jq", ".conclusion"], capture_output=True, text=True)
            runs[r] = p.stdout.strip() or "unknown"
        out[case["id"]] = {"sha_found": bool(shas or unknown), "sha_resolved": shas,
                           "sha_unresolved": unknown, "sha_on_main": on_main,
                           "ci_runs": runs}
    json.dump(out, sys.stdout, ensure_ascii=False, indent=1)


if __name__ == "__main__":
    main()
