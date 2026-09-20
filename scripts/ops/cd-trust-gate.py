#!/usr/bin/env python3
"""CD 信任閘門：正式 daemon 換版之前，確認「上次部署的 sha → 這次要部署的 sha」之間沒有外人的東西。

為什麼要有：repo 是公開的，任何人都能開 PR。main 只有 owner 推得進去，但這台機器上的 bot 都帶著
owner 的 gh 身分與 skip-permissions——被 PR／issue／留言裡的字騙去 merge、cherry-pick 一個 fork PR，
那些 commit 就會照例行更新被建成 binary、在這台機器上以使用者身分跑起來。這支在部署前用確定性的
規則擋下來，叫人來看。呼叫端：daemon-update-kick.sh（申請核准之前）與 daemon-swap.sh（換 binary 之前）。

  cd-trust-gate.py check --repo R --from <已部署 sha> --to <要部署 sha> --state-dir D [--slug owner/name]
      exit 0＝可信；3＝擋下（stdout 一行一個原因）；4＝查不出來（gh 不通等，一樣不准部署）
  cd-trust-gate.py approve --state-dir D --note "為什麼" <sha>...
      使用者看過之後放行個別 commit（完整 sha）。留在 D/cd-trust.approved（JSONL，附時間與說明）。

規則（任何一條不過就擋；已 approve 的 commit 略過 2～4）：
  1. 歷史沒被改寫：--from 必須是 --to 的祖先。
  2. 作者與 committer 的 email 都在 D/cd-trust.allow。檔案不在時以 --from 那個 commit 的兩個 email 建立
     （信任已經在跑的那一版），之後要加人自己編輯。email 可以偽造，所以這條只擋「沒在裝」的，真正的依據是 3、4。
  3. GitHub 說這個 commit 屬於某個 fork 來的 PR（head repo 不是本 repo，或 fork 已刪）。
  4. patch-id 跟任何一個 fork PR 裡的 commit 相同——有人把 fork PR 的內容 cherry-pick／rebase 後直接推 main，
     GitHub 就不會把它跟 PR 關聯起來，所以另外比內容。
  5. 動到部署鏈自己（PROTECTED：閘門、它的測試、從 checkout 直接執行的 daemon-swap.sh／daemon-start.py）。
     這些改了等於可以把閘門關掉，一律要人看過。想多盯幾個路徑（例如 .github/）就在 D/cd-trust.protected 一行寫一個
     前綴；只能加、不能拿掉內建的。daemon-update-kick.sh 不在裡面：正式跑的是手動 install 的那份，repo 裡改了不會生效。

邊界（老實說）：這台機器上的 shell 都能改 D 裡的檔或繞過這兩支腳本；閘門是偵測＋停手＋叫人，不是沙箱。
"""
import argparse
import json
import os
import subprocess
import sys
import time

GIT = "/usr/bin/git"
PROTECTED = ("scripts/ops/cd-trust-gate.py", "scripts/ops/cd-trust-gate_test.sh", "scripts/ops/daemon-swap.sh", "scripts/ops/daemon-start.py")
MAX_COMMITS = 400   # 範圍大到這樣就不逐一問 GitHub 了：直接叫人看


class Unknown(Exception):
    """查不出來（不是不可信）：gh 不通、格式不符。呼叫端一樣不部署。"""


def git(repo, *args, check=True):
    p = subprocess.run([GIT, "-C", repo, *args], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise Unknown("git %s 失敗：%s" % (" ".join(args[:2]), p.stderr.strip()[:200]))
    return p


def gh_json(gh, *args):
    try:
        p = subprocess.run([gh, *args], capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as e:
        raise Unknown("gh 跑不起來：%s" % e)
    if p.returncode != 0:
        raise Unknown("gh %s 失敗：%s" % (" ".join(args[:2]), p.stderr.strip()[:200]))
    try:
        return json.loads(p.stdout or "null")
    except ValueError:
        raise Unknown("gh %s 回的不是 JSON" % " ".join(args[:2]))


def slug_of(repo):
    url = git(repo, "remote", "get-url", "origin").stdout.strip()
    for prefix in ("git@github.com:", "https://github.com/", "ssh://git@github.com/"):
        if url.startswith(prefix):
            return url[len(prefix):].removesuffix(".git").strip("/")
    raise Unknown("origin 不是 github.com 的 repo：%s" % url)


def read_lines(path):
    try:
        with open(path) as f:
            return [l.strip() for l in f if l.strip() and not l.startswith("#")]
    except OSError:
        return None


def approved_shas(state_dir):
    out = set()
    for line in read_lines(os.path.join(state_dir, "cd-trust.approved")) or []:
        try:
            out.add(json.loads(line)["sha"])
        except (ValueError, KeyError, TypeError):
            continue   # 壞掉的一行不算放行
    return out


def allowlist(repo, state_dir, base):
    path = os.path.join(state_dir, "cd-trust.allow")
    lines = read_lines(path)
    if lines is None:
        emails = sorted(set(git(repo, "show", "-s", "--format=%ae%n%ce", base).stdout.split()))
        os.makedirs(state_dir, exist_ok=True)
        with open(path, "w") as f:
            f.write("# CD 信任閘門：可以出現在正式 binary 裡的 commit 作者／committer email，一行一個。\n")
            f.write("# 由 %s（建立當下已部署的版本）帶出來；要加人自己編輯。\n" % base[:12])
            f.write("\n".join(emails) + "\n")
        lines = emails
    return {e.lower() for e in lines}


def patch_ids(repo, rev_range):
    """{patch-id: sha}。空 commit、merge commit 沒有 patch-id，不在裡面。"""
    log = subprocess.run([GIT, "-C", repo, "log", "-p", "--no-merges", "--format=commit %H", *rev_range], capture_output=True)
    if log.returncode != 0:
        raise Unknown("git log -p 失敗：%s" % log.stderr.decode("utf-8", "replace").strip()[:200])
    p = subprocess.run([GIT, "-C", repo, "patch-id", "--stable"], input=log.stdout, capture_output=True)
    if p.returncode != 0:
        raise Unknown("git patch-id 失敗")
    out = {}
    for line in p.stdout.decode().splitlines():
        pid, _, sha = line.partition(" ")
        if pid and sha:
            out[pid] = sha
    return out


def fork_prs(gh, slug):
    rows = gh_json(gh, "pr", "list", "-R", slug, "--state", "all", "--limit", "300", "--json", "number,isCrossRepository,author")
    if not isinstance(rows, list):
        raise Unknown("gh pr list 格式不符")
    return [r for r in rows if r.get("isCrossRepository")]


def check(a):
    repo, base, head = a.repo, a.base, a.head
    for rev in (base, head):
        if git(repo, "cat-file", "-e", rev + "^{commit}", check=False).returncode != 0:
            raise Unknown("本機沒有 commit %s" % rev)
    if git(repo, "merge-base", "--is-ancestor", base, head, check=False).returncode != 0:
        return ["歷史被改寫：已部署的 %s 不是 %s 的祖先" % (base[:12], head[:12])]

    commits = git(repo, "rev-list", "%s..%s" % (base, head)).stdout.split()
    if not commits:
        return []
    if len(commits) > MAX_COMMITS:
        return ["範圍有 %d 個 commit（上限 %d），不逐一查了，請人工看過再 approve" % (len(commits), MAX_COMMITS)]
    approved = approved_shas(a.state_dir)
    allow = allowlist(repo, a.state_dir, base)
    problems = []

    # 5：部署鏈自己。看的是每個 commit 動到的檔，不是頭尾 diff——改了再改回來也要看過。
    protected = PROTECTED + tuple(read_lines(os.path.join(a.state_dir, "cd-trust.protected")) or [])
    for sha in commits:
        if sha in approved:
            continue
        files = git(repo, "show", "--format=", "--name-only", "-m", sha).stdout.split("\n")
        hit = sorted({f for f in files if f and f.startswith(protected)})
        if hit:
            problems.append("%s 動到部署鏈（要人看過）：%s" % (sha[:12], "、".join(hit[:4])))

    # 2：email。
    for sha in commits:
        if sha in approved:
            continue
        ae, ce = git(repo, "show", "-s", "--format=%ae%n%ce", sha).stdout.split()[:2]
        bad = [e for e in (ae, ce) if e.lower() not in allow]
        if bad:
            problems.append("%s 的作者／committer 不在允許名單：%s" % (sha[:12], "、".join(sorted(set(bad)))))

    # 3、4：GitHub 上的 fork PR。
    slug = a.slug or slug_of(repo)
    forks = fork_prs(a.gh, slug)
    fork_numbers = {r["number"] for r in forks}
    for sha in commits:
        if sha in approved:
            continue
        pulls = gh_json(a.gh, "api", "repos/%s/commits/%s/pulls" % (slug, sha))
        if not isinstance(pulls, list):
            raise Unknown("commits/%s/pulls 格式不符" % sha[:12])
        for pr in pulls:
            head_repo = ((pr.get("head") or {}).get("repo") or {}).get("full_name")
            if pr.get("number") in fork_numbers or head_repo is None or head_repo.lower() != slug.lower():
                who = (pr.get("user") or {}).get("login") or "?"
                problems.append("%s 來自 fork PR #%s（%s，head=%s）" % (sha[:12], pr.get("number"), who, head_repo or "fork 已刪"))
    if forks:
        mine = patch_ids(repo, ["%s..%s" % (base, head)])
        for pr in forks:
            n = pr["number"]
            ref = "refs/cd-trust/pr-%s" % n
            if git(repo, "fetch", "-q", "origin", "+refs/pull/%s/head:%s" % (n, ref), check=False).returncode != 0:
                raise Unknown("抓不到 fork PR #%s 的 head，沒辦法比內容" % n)
            theirs = patch_ids(repo, [ref, "--not", base, "--max-count=200"])
            for pid, their_sha in theirs.items():
                sha = mine.get(pid)
                if sha and sha not in approved:
                    who = (pr.get("author") or {}).get("login") or "?"
                    problems.append("%s 的內容跟 fork PR #%s（%s）的 %s 相同，但不是經由那個 PR 進來的" % (sha[:12], n, who, their_sha[:12]))
    return sorted(set(problems))


def approve(a):
    os.makedirs(a.state_dir, exist_ok=True)
    bad = [s for s in a.shas if len(s) != 40 or any(c not in "0123456789abcdef" for c in s)]
    if bad:
        sys.exit("要完整的 40 碼 sha：%s" % " ".join(bad))
    with open(os.path.join(a.state_dir, "cd-trust.approved"), "a") as f:
        for sha in a.shas:
            f.write(json.dumps({"sha": sha, "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "note": a.note}, ensure_ascii=False) + "\n")
    print("approved %d" % len(a.shas))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check")
    c.add_argument("--repo", required=True)
    c.add_argument("--from", dest="base", required=True)
    c.add_argument("--to", dest="head", required=True)
    c.add_argument("--state-dir", required=True)
    c.add_argument("--slug", default=os.environ.get("CD_TRUST_SLUG", ""))
    c.add_argument("--gh", default=os.environ.get("CD_TRUST_GH", "gh"))
    p = sub.add_parser("approve")
    p.add_argument("--state-dir", required=True)
    p.add_argument("--note", required=True)
    p.add_argument("shas", nargs="+")
    a = ap.parse_args()
    if a.cmd == "approve":
        return approve(a)
    try:
        problems = check(a)
    except Unknown as e:
        print("查不出來：%s" % e)
        sys.exit(4)
    if problems:
        print("\n".join(problems))
        sys.exit(3)


if __name__ == "__main__":
    main()
