# AGM ops scripts (`scripts/ops/`)

The scripts that run in production used to live only in `~/.config/agents-manager/supervisor/AGM/bin/`, outside version
control: no review, no history, nothing to diff against when one was broken. This directory is their source of truth.

**Nothing here is installed automatically.** The daemon only deploys `bin/agm` (`scripts/agm.py`) into the supervisor
(AGM) working directory. Whether and when a kick script is installed is AGM's call, made inside a maintenance window.
Commands marked "needs AGM approval" below follow that rule.

All paths below are relative to the repo root unless they start with `~`. `AGM_DIR` means
`~/.config/agents-manager/supervisor/AGM`.

## What is installed, and how far behind is it (`agm ops-sync`)

`install-manifest.tsv` is the only list of installable files and their destinations. `bin/agm ops-sync --check`
compares the installed copies (in `AGM_DIR`) read-only against the local repo's `origin/main` (`git fetch` first for the
latest; `--repo` defaults to `AGM_REPO`, then `~/project/agents-manager`) and reports four kinds of difference:

| Kind | Meaning |
| --- | --- |
| `drift` | The installed file matches no version in the repo — someone edited the installed copy directly. The most serious kind. |
| `behind` | The repo is newer than what is installed. Reports which commit is installed and the titles of the commits it is missing. Scripts are compared byte for byte, plists/units semantically, and **both search history**: an installed copy equal to an older repo version is `behind`, not `drift`. |
| `missing` | Listed in the manifest, absent from the install directory. |
| `extra` | Present in `bin/` but not in the manifest (a script with no version control). `agm` and `*.bak*` are exempt. |

An optional third manifest column, `darwin` or `linux`, restricts a row to that platform (omitted = both). Rows for the
other platform are listed under `skipped` and do not count as a difference; the `platform` field says which side the
check used. Schedule rows must be labelled correctly (`LaunchAgents/…` → `darwin`, `systemd/…` → `linux`); a wrong label
is `bad_manifest` (exit 2), not a pile of `missing`.

Exit 0 means in sync; exit 1 means differences, and `--alert` additionally pushes one `ops_alert`
(`source=ops-sync`, `reason=installed_out_of_sync`, at most one per hour). On agm-host `ubuntu-ci.sh` schedules the
check (every `AGM_CI_OPS_SYNC_INTERVAL` seconds, default 6 hours, independent of new commits; `--alert` wakes the
supervisor; the result is kept in `~/.cache/agents-manager/ci/ops-sync.json`). It only detects; it never installs.

`--check` also compares **`bin/agm`** read-only (issue #532). The `installed` section means "the installed copy is not
the one embedded in this binary" — `--refresh-cli` replaces it in place (`POST /api/supervisor/cli`, no daemon restart
needed). The `binary` section means "the copy embedded in the binary is behind the repo" — that needs a rebuilt binary
**and a daemon restart**, which this command cannot do. If the daemon cannot be queried, `cli.state` is `unknown` and the
ops-script half of the verdict is unaffected.

## Automatic refresh of installed scripts (`ops-install.sh`, flag, off by default)

Root cause (2026-10-01, `outbox-gc.sh` stuck on a 9/28 version): installation was manual, and the deploy flow only
handles the daemon binary — a change to ops scripts never produces a new binary (`daemon-update-kick.sh` sees "no
differences that land in the binary" and stops), so nothing ever copied them over.

```sh
scripts/ops/ops-install.sh --repo <git dir> --ref <rev> --dir <AGM dir> [--platform linux|darwin] [--dry-run] [--force]
```

- Reads only the rows of `install-manifest.tsv` (at `--ref`) for this platform; sources come from `git show <ref>:<path>`.
- **Updates only files that are already installed and differ from the repo.** A file absent from the install directory is
  reported `not-installed` (the first install is manual); schedule units (`systemd/`, `LaunchAgents/`) are reported
  `skipped` and left alone (they also need `daemon-reload`/`launchctl`); files outside the manifest are never touched.
- Each file is first written to a temp file in the same directory and **self-checked there** (`.sh` → `bash -n`,
  `.py` → byte-compile, `.ts` → `bun build` if bun is present). A failure discards the temp file, reports `failed`, leaves
  the installed file byte-identical, and carries on with the rest. A pass backs the old file up to
  `<AGM>/ops-install-backups/<UTC time>/<install path>` and then `mv`s the new one into place (atomic).
- A previous run killed by SIGKILL leaves `<install path>.new.<pid>` behind. The next non-`--dry-run` run (after taking the
  lock) removes manifest-listed `.new.<digits>` files whose pid is gone and prints `cleaned N` (live pids are left
  alone). `ops-sync --check` does not report such names as `extra`.
- One run at a time: an OS advisory lock on `<AGM>/ops-install.lock.guard` serializes lock-directory creation and stale
  recovery, and `<AGM>/ops-install.lock/` records the pid (takeover is possible after SIGKILL). If the lock cannot be
  taken it exits 3 without touching anything. Repeated installs in the same second get distinct backup directories
  (`<UTC time>-2`, …). An install path that is a symlink is `skipped` (`mv` would replace the link with a copy); a parent
  directory that is a symlink, or a manifest destination escaping `--dir` (absolute path, `..`), is `failed` and never written.
- **Hand-edited files are not overwritten.** If the installed file matches no version in `git log <ref> -- <source>`
  (the same test as `drift` in `ops-sync --check`), or its content changes during the self-check/backup, it is reported
  `drifted`, left untouched, and is not a failure; the kick also pushes `ops_install_drift` so a person looks. `--force`
  replaces it anyway (still backed up).
- `--dry-run` only lists `would-install`. The last line is `changes=N failed=M drifted=K`; any failure exits 1. A fully
  successful run that really replaced files writes "time commit" to `<AGM>/ops-install.last`; a run with failures does
  **not** update it and instead records "time commit failed=N" in `ops-install.last-failed` (deleted by the next fully
  successful run). Nothing reads these two files (neither the kick nor `ops-sync`); they are for humans asking "which
  version did we get to".

**Wiring into deploys.** With the flag on, `daemon-update-kick.sh` runs the target sha's own `ops-install.sh` (1) after a
successful swap and (2) in the "no differences that land in the binary" round (which requires a green `ubuntu-ci` for
that sha). A failure pushes `ops_alert` (`ops_install_failed`) and does not affect the deploy result. **The flag is off by
default**; the user or AGM turns it on:
`touch ~/.config/agents-manager/supervisor/AGM/ops-auto-install.enabled` (or give the kick `AGM_OPS_AUTO_INSTALL=1`);
delete the file to turn it off. The kick itself is an installed file, so install this version of the kick manually once
before the flag has anything to act on.

## Schedules: launchd and systemd

### launchd (macOS; issue #487)

`scripts/ops/launchd/com.agm.*.plist` are the source files for the 8 jobs on the dev Mac, and `install-manifest.tsv`
lists them (destination `LaunchAgents/…`, which `ops-sync --check` resolves to `~/Library/LaunchAgents/`).

The comparison uses the `plistlib`-parsed dict (**key order is irrelevant**) and compares **everything except
`EnvironmentVariables`**, including `StandardOutPath`, `StandardErrorPath` and `WorkingDirectory`.
`EnvironmentVariables` is skipped because it holds per-machine values (`PATH`, bot ids); the repo copy keeps them only so
the job still runs after an install.

(#487 first used an allow-list on the theory that "launchd rewrites plists". That was wrong: the real files are plain XML
that launchd only reads. The "different key order" seen then was `PlistBuddy -c Print` output order. The allow-list
silently ignored unlisted keys — `StandardOutPath`/`StandardErrorPath` among them — and `browser-gc-kick.sh` has no
ops-alert channel, so a drifted log path would have shown green. Issue #499.)

A `com.agm.*` job in `~/Library/LaunchAgents/` that is not in the manifest is reported `extra` (a schedule with no
version control). Paths and `gui/501` are hard-coded to the dev machine, as in `herdr-full-restart.sh`.
**This only covers version control and comparison; it does not install or call `launchctl`.**

### systemd user units (Linux; issue #677)

On the Linux host (#675) the same jobs are scheduled by the systemd user manager: `scripts/ops/systemd/com.agm.<name>.service`
(`Type=oneshot`, runs the same installed kick) plus `.timer`, listed in the manifest as `systemd/…` (resolved to
`~/.config/systemd/user/`, or `$XDG_CONFIG_HOME` when set). The mapping below is pinned by `SystemdParityTest` in
`scripts/agm_test.py` — change one side and forget the other and it goes red:

| launchd | systemd |
| --- | --- |
| `ProgramArguments` | `ExecStart=` (same interpreter; `/Users/<user>/` becomes `%h/`; bun is `%h/.bun/bin/bun` on Linux) |
| `StartInterval N` | timer `OnUnitActiveSec=Ns`, first run `OnActiveSec=Ns` (launchd also waits one interval after load) |
| `RunAtLoad` | `OnActiveSec=1s` (run once as soon as the timer starts) |
| `StandardOutPath` / `StandardErrorPath` | `StandardOutput=` / `StandardError=append:…/<name>.systemd.log` |
| `EnvironmentVariables` | `Environment=` (same variable names; values are per-machine and not compared by `ops-sync`) |
| a job never overlaps itself | the timer does not start a new run while the unit is still running |

One systemd-only point: **units are reaped by cgroup, not process group.** The `dev-server` kick starts vite detached;
under launchd leaving the process group is enough to survive, but systemd's default `KillMode=control-group` would kill
vite the moment the kick ends, so that unit sets `KillMode=process`.

`ops-sync` compares units as parsed "section.key → values (in order)": comments, blank lines and trailing-`\`
continuations do not count, and **everything except the values of `Environment=` is compared** (the key must exist;
same reasoning as the plist). On Linux it scans `~/.config/systemd/user/com.agm.*` and not `~/Library/LaunchAgents`.

The Linux `com.agm.browser-gc` timer runs the Linux-only `browser_gc_linux.py` (see the GC section below).
`browser-gc-kick.sh`, `browser-gc-task.md` and the plist stay `darwin`-only; the ego lite/OB GUI worker is deliberately not
implemented on Linux (#718), so a macOS `browser-gc-kick.sh` found in a Linux `bin/` is still reported `extra`.

Install (**needs AGM approval**, same as macOS). `<name>` is any unit in the manifest that has both a `.service` and a
`.timer` (currently ten); `daemon-boot` has no timer (see below) and `disk-gc` is also covered in its own section:

```sh
install -m 644 scripts/ops/systemd/com.agm.<name>.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now com.agm.<name>.timer
loginctl enable-linger "$USER"   # once; without it the whole user manager stops when you log out
```

**Starting the daemon at boot (issue #858, Linux).** `com.agm.daemon-boot.service` (oneshot, no timer) plus
`bin/daemon-boot.sh`: if nothing listens on 7788 at boot it starts the production daemon through `systemd-run --user`
(`Type=forking`, `KillMode=process`, same as `daemon-swap.sh`); if the daemon is already up it does nothing.
`herdr@.service` also has an `[Install]` section; enable it only for the session the daemon uses:

```sh
install -m 755 scripts/ops/daemon-boot.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/systemd/com.agm.daemon-boot.service scripts/ops/systemd/herdr@.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable com.agm.daemon-boot.service herdr@agents-manager.service
loginctl enable-linger "$USER"   # required: the user manager must start at boot without a login
```

To update `herdr@.service` on an installed host, reinstall the source file and reload by hand; `ops-sync` only reports
the difference and never installs or reloads:

```sh
install -m 644 scripts/ops/systemd/herdr@.service ~/.config/systemd/user/
systemctl --user daemon-reload
```

`OOMPolicy=continue` keeps the herdr server alive when the OOM killer terminates a process inside a pane. The setting only
takes effect at the next herdr restart; do not restart herdr just for this — a restart interrupts every pane.

Acceptance: after `qm shutdown 114 && qm start 114`, with no manual step, `curl 127.0.0.1:7788/api/session` answers and
the supervisor has re-attached its bots. Isolated test: `bash scripts/ops/daemon-boot_test.sh` (fake `systemd-run`,
`curl`, `agm`; HOME pointed at a temp dir).

The `daemon-update` unit's `Environment=` carries a PATH
(`%h/.local/bin:%h/.bun/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`) and `HERDR_SESSION=agents-manager`.
If a new host keeps its tools elsewhere, edit the installed copy; `ops-sync` will not flag it (`Environment=` is checked
for presence only, not content, so a missing `HERDR_SESSION` on the installed side is not reported either — reinstall or
add a drop-in yourself). The macOS `daemon-update` PATH must also keep `~/.local/bin`, because `daemon-swap.sh` calls the
herdr installed there. Only the systemd unit sets `HERDR_SESSION`: a Linux host runs just the daemon's session, with no
default server and no pane environment in a scheduled run, so without it `daemon-swap.sh`'s `herdr pane list` would return
`server_not_running` and abort the swap. If the daemon moves to another session name, change it here too.

## daemon-update-kick.sh

Routine automatic deploy (simplified 2026-09-29 by the user). When the production daemon's release binary is behind the
latest `origin/main` commit with a green `ubuntu-ci`, **the script itself** builds and swaps — no LLM, no approval ticket,
no build child. The build was tested before the push to main and `ubuntu-ci` already runs the whole tree in the
background, so AGM does not verify again.

Flow (SPEC §18.2): `git fetch` in the dedicated checkout → compare with `daemon-update.built`; if nothing that lands in the
binary differs, stop → walk back along first-parent to the newest sha whose `ubuntu-ci` is `success`
(`gh api repos/Eden-Sun/agents-manager/commits/<sha>/status`) → `bun run build` plus
`cargo build --release -p agents-managerd` in the dedicated checkout → swap with `daemon-swap.sh`.

It runs every 5 minutes (`StartInterval 300` / `OnUnitActiveSec=300s`). With nothing new, a round is a single
`git fetch` and never asks GitHub.

Environment variables (all optional; the unit/plist `Environment` carries only `PATH` — plus `HERDR_SESSION` for the unit,
see above — which must reach `git`, `gh`, `bun`, `python3` and `nice`):

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | Supervisor working directory (`bin/agm`, logs and state live here) |
| `AGM_REPO` | `~/project/agents-manager` | The checkout the production daemon runs from (`daemon-swap.sh` swaps its `target/release`); also the source of the dedicated checkout's origin URL |
| `AGM_DEPLOY_CHECKOUT` | `~/.cache/agents-manager/deploy-checkout` | Dedicated clean checkout (cloned on first use). Only this script touches it — never the main tree or anyone's worktree; `target/` and `node_modules` stay so builds are incremental |
| `AGM_GH_REPO` | `Eden-Sun/agents-manager` | Repo to ask for commit status |
| `AGM_CI_CONTEXT` | `ubuntu-ci` | Which status context to read |
| `AGM_CI_LOOKBACK` | `30` | How many first-parent commits to look back (`ubuntu-ci` only runs the newest HEAD and skips shas in between) |
| `AM_AGENT_NAME` | `daemon-update-kick` | Lease owner / `ops_alert` source |
| `AGM_FAIL_ALERT_AFTER` | `6` | Consecutive rounds that could not finish before `ops_alert check_failing` (5 min each ≈ 30 min) |
| `AGM_LOCK_STALE_SECS` | `120` | With no discoverable owner, a lock older than this is reclaimed as stale; never reclaimed while the OS advisory lock is still held |
| `AGM_LOCK_HUNG_SECS` | `7200` | Owner alive but stuck this long: push `ops_alert runner_hung` (the lock is not stolen; a cold build takes over ten minutes, so this is generous) |
| `AM_MAINTENANCE_ESCALATE_MINS` (daemon side) | `30` | Only meaningful for approvals a bot requested (SPEC §18.10); tickets the auto-deploy opens itself have a wait of 0 and never escalate |

State files (all in `AGM_DIR`): `daemon-update.built` (short sha last swapped in, written by `daemon-swap.sh`),
`daemon-update.rejected` (full shas that were swapped in and rolled back; never picked again), `daemon-update.fails`
(consecutive failed rounds), `daemon-update.lock` (owner and diagnostics), `daemon-update.lock.guard` (kernel-released
advisory lock preventing overlap), `daemon-update.now.json` (a deploy-now request), `daemon-update.log`.
Each `daemon-swap.sh` run leaves `agents-managerd.bak-<old sha>` (for rollback) and
`.prev-<old sha>-<time>-<six random chars>` (mktemp, so two swaps in the same second never collide) in `target/release/`
(the binary that was replaced). A **successful** swap keeps only this run's two and deletes the older ones (DB backups
likewise keep only the newest); rollback and abort clean nothing.
If `.built` is missing or points to a sha not in the repo, the script pushes `ops_alert built_unknown` and stops — it must
know which version is live before it dares to move forward.

A sha already built (`<checkout>/target/release/.built-for`) and waiting for a safe window is not rebuilt on later rounds.
`daemon-swap.sh` exit 4 (someone is busy) is not a failure — try again next round; a failed service-identity check (token
file missing, capability probe fails) is 3 and does count, since a misconfiguration will not fix itself (issue #1178).
Other exit codes: 10 (aborted **before** the swap: the new binary's embedded `--version` sha is not the target, it was built
from a dirty tree, or the old binary has no embedded sha; nothing was touched) pushes `swap_binary_sha_mismatch` and is
not recorded in `.rejected`; 6 (schema was upgraded, fixed forward) back-fills `.built` and pushes `swap_forward_fixed`;
7 (rolled back) pushes `swap_rolled_back` and records the sha in `.rejected`; 8 (swapped, but the window was not handed
back) pushes `swap_lease_not_released`; 9 (live daemon too old, no restart-window route) pushes `swap_daemon_too_old`;
12 (after a rollback the old binary would not start — `/api/session` is unreachable for 30 s) does not count as rolled
back; the kick applies the default failure count and a human must step in (issue #1102).

**First deploy / old daemon.** The automatic kick carries no approval id; if the live daemon lacks
`POST /api/services/daemon-swap/restart-window`, it aborts with 9 and reports. When running an already-approved legacy
deployment, call `daemon-swap.sh --approval <id>` from the same checkout: only a detected old daemon (`service_old` /
`bootstrap`) bootstraps with the User token and the old approval lease, and `service_old` also needs an existing service
token (a missing file fails closed). When the new route exists the parameter is accepted but ignored, and the service
principal is used. After the first successful swap the new daemon creates the service token and later automatic swaps use
the new path. The script first checks that `daemon-start.py` from the same checkout is readable, and otherwise aborts with 3
before stopping the daemon.

### Deploy now (user decision 2026-09-25, SPEC §18.2)

When the user presses "Deploy now" at the top left of the web UI, the daemon (`POST /api/deploy/now`) validates the target,
writes `daemon-update.now.json` (`{sha,live_sha,requested_at,requested_by}`; **no approval ticket**), and then runs
`launchctl kickstart gui/<uid>/com.agm.daemon-update` (Linux: `systemctl --user start --no-block com.agm.daemon-update.service`
— `--no-block` because starting a oneshot would wait for the whole kick round; if the daemon has no `XDG_RUNTIME_DIR` it
fills in `/run/user/<uid>`, issue #677). The kick then reads the file:

- It deploys **that sha without waiting for `ubuntu-ci`** (pressing the button is an explicit request); everything else
  (build, `daemon-swap.sh` safety conditions) is the same as the routine path.
- The sha must be an ancestor of origin/main (otherwise `ops_alert now_target_invalid`) and the live version must be an
  ancestor of it (otherwise `now_target_older`, which refuses a downgrade). If there is no code difference between `.built`
  and the target, the request is dropped (already current).
- The request file is deleted only after the work finishes (including a fix-forward or rollback); `daemon-swap.sh`
  returning 4 (someone is busy) keeps it for the next round. A corrupt file → `ops_alert now_request_corrupt` and deletion.

The daemon decides `kick_ready` by whether the installed kick contains the string `daemon-update.now.json`, so **install
this version of the kick before the button works**.

### Isolated test

Never test against the production daemon. Start a separate daemon with its own data directory:

```sh
AM_DATA_DIR=/tmp/am-ops-test ./target/release/agents-managerd serve --port 7799 &
mkdir -p /tmp/am-ops-test/supervisor/AGM/bin
# Point bin/agm at the test daemon (write 127.0.0.1:7799 as daemon_url in runtime.json)
AGM_DIR=/tmp/am-ops-test/supervisor/AGM AGM_REPO=$PWD AGM_DEPLOY_CHECKOUT=/tmp/am-ops-test/checkout \
  AGM_SWAP_SCRIPT=/bin/true bash scripts/ops/daemon-update-kick.sh
cat /tmp/am-ops-test/supervisor/AGM/daemon-update.log
```

### Guardrails for destructive commands (issue #422 — required reading)

Each `*_test.sh` copies the script under test and really runs it, and those scripts contain `launchctl bootout`,
`pkill -f` and `ssh`. At 2026-09-23 17:39Z one test's PATH did not block them and really booted out two herdr launchd jobs
under `gui/501`; every pane on the machine vanished. So `scripts/check.sh ops` runs each test under
`scripts/ops/destructive-canary.sh`, which intercepts in three layers:

1. **PATH**: the canary directory goes first. Cheap, but **useless once a script overwrites PATH wholesale**.
2. **Exported bash functions**: functions win over PATH lookup, survive a PATH override, and follow the environment into
   child bashes. `export -f` is bash-only — in zsh it prints the definition, which looks like success but exports nothing.
3. **zsh `$ZDOTDIR/.zshenv`**: zsh reads it for every shell (`zsh -c` and nested ones included), independent of PATH.
   Without this layer a child zsh has no protection once PATH is overridden.

`rm` is not blocked wholesale (tests must clean their own temp directories); only targets that are **absolute paths outside
the temp directory** are refused — which is exactly what `rm -rf ~/foo` expands to. Set `AM_CANARY_ALLOW_RM` to allow
another root.

**What the guardrail cannot catch (measured; having three layers does not make this safe):**

- **Absolute-path invocations** (`/bin/launchctl`): they bypass PATH and shell function lookup, so all three layers miss
  them. **Never call a destructive command by absolute path**; if you must choose one, go through an indirection variable
  such as `LAUNCHCTL_BIN=` pointing at a stand-in (`daemon-swap_test.sh` does this).
- **Non-shell child processes** (Python `subprocess`, `Bun.spawn`, anything using `execvp`): layers 2 and 3 protect the
  *shell's* command lookup, and `execvp` never goes through the shell, so only the PATH layer applies. Measured: with a
  complete PATH it blocks, with an overridden PATH Python and Bun both slip through (`FileNotFoundError` /
  `Executable not found in $PATH` only because the test used a sentinel; a real command would run).
  ⇒ **When a test invokes a `.py`/`.ts`, that line's PATH must include `${AM_CANARY_DIR:+$AM_CANARY_DIR:}`.**
  `lint` scans for an interpreter plus script on one line; invocations through a variable (`"${BUN}" "${SCRIPT}"`) are not
  detected, so this rule relies on discipline. One level deeper — a test that runs a `.py` which itself spawns — is
  invisible to `lint`; check the PATH yourself when writing such a test.
- **`env -i`** clears the exported functions and `ZDOTDIR`, leaving only the PATH you give on that line.

The `rm` guard's boundary: the target is normalized to an absolute path and symlinks in the path are resolved before
comparing, so `cd /tmp && rm ../Users/…` and "a symlink under `/tmp` pointing outside the temp directory" are both
blocked. **Only the directory part is resolved**: if the last component is itself a symlink, the link is what is compared
(deleting a link does not delete its target, which is correct).

When writing a new test:

- Include the canary in PATH: `PATH="$your_fakebin:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin"`.
- If you genuinely need a minimal environment (to imitate launchd, say), put
  `# canary-gap: <reason, and how the destructive commands in that section are blocked another way>` on the line above;
  `lint` then lets it through. The point is that **someone wrote down a reason**, rather than silently leaving a hole.
- `scripts/ops/canary-baseline.tsv` is a ratchet: existing gaps are recorded there and can only shrink. After fixing one,
  run `scripts/ops/destructive-canary.sh lint --update` to lower the count.

**When verifying the guardrail itself, use only the harmless sentinel command name `am-canary-probe`.** Never use a real
`launchctl`/`pkill` as a guinea pig: if the guardrail fails, the guinea pig becomes real damage — that is exactly how #422
happened.

### Production install (needs AGM approval)

```sh
install -m 755 scripts/ops/daemon-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

`daemon-swap.sh` and `daemon-start.py` are not installed: the kick runs them from the dedicated checkout (the sha being
swapped in).

## daemon-swap.sh (plus daemon-start.py)

The step of the automatic deploy that replaces the binary: take the restart window → back up the DB → swap the binary →
restart → verify → write `.built`. It used to be a throwaway script written per run by the build child; on 2026-09-20 one
such script hard-coded `user_version` as 10 (that batch moved to 11), judged the swap failed, rolled back, the old binary
was blocked by the version gate, and the daemon was down for 33 seconds. So it is under version control and its behaviour
is pinned by `daemon-swap_test.sh`:

- The expected schema version is read from the checkout's `SCHEMA_HISTORY`, never hard-coded; unreadable means abort.
- Before restoring the DB in a rollback, stop the daemon and remove `-wal`/`-shm`; after restoring, self-check
  `user_version` and `integrity_check`.
- After all deploy verification succeeds, keep this run's DB backup and delete the older `.bak-*` of the same DB; abort and
  rollback never clean up backups.
- Stopping the daemon always goes through `stop_daemon`: exactly one matching old process must be found before the swap;
  TERM waits up to 30 s, then KILL and confirm the pid is gone. Not found, several found, or still alive after KILL aborts
  with rc=11 without overwriting the binary or restoring the DB. A rollback (including the restart of a fix-forward) that
  cannot stop the daemon also preserves the scene.
- A failure after a schema upgrade **fixes forward by default** (keep the new binary, exit 6); only if the new binary
  cannot start are binary and DB restored (exit 7).
- The daemon is started with `launchctl submit` + `daemon-start.py` (fork + setsid), so it ends up ppid=1, nice 0.
  Backgrounding it inside a pane would inherit the pane's busy nice 5, which a non-root user cannot lower.
  On Linux (issue #677) it runs the same `daemon-start.py` through
  `systemd-run --user --collect --unit=am-daemon-swap-<pid>-<attempt> -p Type=forking -p KillMode=process`: the transient
  unit stands in for `launchctl submit`, so no unit file needs installing. `Type=forking` makes systemd treat the forked
  daemon as the main PID; `KillMode=process` is needed because systemd reaps by cgroup, which setsid cannot escape, and
  the default would kill the daemon's child processes too. The platform comes from `uname -s` (tests override it with
  `AGM_OPS_PLATFORM`); without `XDG_RUNTIME_DIR` it fills in `/run/user/<uid>`.
- Step 3b sends a self-test prompt to a fixed probe bot: `SWAP_PROBE_BOT` > `swap_probe_bot_id` in `AGM_DIR/runtime.json` >
  `manager_bot_id` in the same file (the AGM supervisor itself: resident, always present; issue #882). If the target has
  **no active run** (`no active run`) the step is skipped, logged and counted in `daemon-swap.probe-skips` (the kick reads
  it to raise an alert) without blocking the deploy; any other non-200 aborts. With none of the three available, 3b is
  skipped through the same path.
- Before swapping, a non-empty `agm state` bot list must be readable; a failing command or an empty list exits 3 and
  hands back the restart lease.
- After the restart the bot list (by id) is compared: a missing bot rolls back, **unless** it was deliberately deleted
  inside the swap window — `deleted_at` after the window start **and** a `delete_bot`/`delete_project` intent left by the
  delete API (subject is the bot, its project, or the payload snapshot contains it), checked with a read-only DB query.
  A `deleted_at` without an intent (retired by reconcile after the restart, projection soft delete) still rolls back
  (issue #553: on 2026-09-24 a parent bot deleting child i263 inside the window was misjudged as a rollback case).
  When a parent closes a child with `herdr pane close` (no DELETE call), the daemon's retirement writes a `retire_child`
  record; only entries whose subject is that bot, inside the window, with `cause` `pane_closed` (herdr reported the close
  event and the pane is currently absent) or `promoted` count. For boot-reconcile `unconfirmed` children there is a narrow
  exception: if the intent says the pane is gone, the parent bot still has an active run and remains in the after-list,
  the script re-reads for up to 5 s waiting for the intent to land. `agent_missing` where the parent vanished but the pane
  remains, `herdr_restarted`, and any other `unconfirmed` still roll back (issue #834; criteria in SPEC §6.5a).
- A failed `agm supervisor` read, an empty status, or one not in `starting`/`idle`/`busy` rolls back; the post-swap bot
  list must also be read successfully and be non-empty. Exception (issue #771): the status is read once before the daemon
  is stopped, and if it was `waiting_quota` **before** the swap and still is after, it is treated as a quota wait unrelated
  to the new version and does not roll back (the log says so). Healthy before and `waiting_quota` after, or an unreadable
  "before", still rolls back. A sha that earlier landed in `daemon-update.rejected` because of this rule (09a2438d on
  2026-10-02) is not released automatically: once you are sure it was innocent, delete that line from the file
  (`grep -vx <full sha> daemon-update.rejected`) and the next round will pick it up again.

```sh
scripts/ops/daemon-swap.sh --sha <full sha> --old <short sha> --old-hash <first 16 hex of sha256> \
    --owner <window owner name> --checkout <clean checkout> [--approval <legacy approval id>]
```

**No approval ticket is needed** (user, 2026-09-29): the window is opened by the daemon's
`POST /api/services/daemon-swap/restart-window` (the daemon-swap service principal opens an immediately-approved restart
ticket itself and then goes through the same acquire — only obtainable when nobody is working or has a delivery in flight
and no one else holds a lease; once granted, assignment dispatch pauses). When there is no pane of its own (a scheduled
run), step 3b uses `herdr pane list` to confirm the socket answers and the protocol matches.

Exit codes: 0 success; 2 bad arguments; 3 preflight failed (including a missing launcher, which guarantees the old daemon
has not been stopped); 4 no window / re-check unsafe; 5 backup problem; 6 stopped on the new binary after fixing forward;
7 rolled back; 8 swapped but the restart window was not handed back (issue #477); 9 daemon too old and no explicit
`--approval` (the automatic kick passes none); 10 the new binary's embedded sha is not `--sha` (aborted before the swap,
nothing touched); 11 could not prove the old daemon exited (binary and DB untouched). **The window does not go away by
itself**: it lasts until the lease's `expires_at` — 900 s by default (`maintenance::DEFAULT_TTL_SECS`, at most 3600, and not
later than the approval's own expiry; a self-opened ticket is valid for ttl + 5 minutes), during which supervisor assignment
dispatch is paused and nobody else can take the restart window. Seeing 8 means someone must act: wait for the TTL, or ask
AGM to take over with `lease release restart --force` and a reason; do not treat it as a clean finish.

The `lease_token` **never appears in argv** (issue #477). After the window is granted, `mktemp` creates a 0600 file under
AGM's private directory (unpredictable path, not in a world-writable /tmp); `agm lease release` reads it through
`--lease-token-file`, and the script deletes the file when it ends, success or not. argv is public to every process of the
same uid (`ps`), and the token is a one-time credential that appears once and cannot be looked up through any API — whoever
copies it can release the window while someone else is mid-swap. The release rc is also no longer swallowed by
`>/dev/null 2>&1`: it used to log "window released" on failure while the window was in fact held until the TTL.

## release-triage-kick.sh and claude-release-kick.sh

Upstream release triage (issue #204): whenever claude or codex ships a version, entries in the changelog that "may be worth
adopting or must be guarded against" go to a dedicated bot for a verdict on each, and the daemon files the actionable ones
as GitHub issues. Read-only and dispatch-only: it does **not upgrade, change settings or restart** — upgrades still follow
§6.9. Version comparison, changelog slicing and bucketing (dropped/kept/unmatched) are all done daemon-side by
`agents-managerd release-triage-check --kind <k> --json`; the script does no comparison or slicing and only follows the
returned JSON (`{"kind","from","to","pending":[{"version","kept","unmatched","dropped_count"}]}`) to decide whether to
dispatch. With an empty `pending` and no new Claude binary it exits quietly without a log line. If a new version appears
in `CLAUDE_VERSIONS_DIR`, the kick appends the binary-diff task and the old/new paths to the same message body, and
dispatches a binary-only assignment even when the changelog has nothing pending. The task body is in
`release-triage-task.md`, the Claude binary diff instructions in `claude-release-diff-task.md`, and `release-issue-task.md`
is the body for working an opened guard/adopt issue; `claude-release-task.md` is kept as the manual parsing entry point
for the update dialog.

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | Supervisor working directory (`bin/agm`, logs and state; the task files live here too) |
| `AGM_REPO` | `~/project/agents-manager` | Repo to find `target/release/agents-managerd` in (fully overridden by `AM_BINARY`) |
| `AM_BINARY` | `$AGM_REPO/target/release/agents-managerd` | Binary used to call `release-triage-check` |
| `AGM_RELEASE_BOT` | `release_bot_id` in `runtime.json`, else `responder_bot_id` | Whom to dispatch to; **never the supervisor itself**. If none is found it skips rather than guessing |
| `AGM_TRIAGE_QUOTA_MAX` | `85` | Do not dispatch when the target bot's 5h usage is at least this percentage |
| `AGM_LOCK_STALE_SECS` | `120` | With no discoverable owner, a lock older than this is reclaimed; not reclaimed while the OS advisory lock is held |
| `AGM_LOCK_HUNG_SECS` | `3600` | Owner alive but stuck this long: push `ops_alert` (the lock is not stolen) |
| `AGM_LOCK_QUIET_SECS` | `60` | A lock younger than this makes a second runner skip silently; only older locks log "already has an owner" |
| `AGM_EXTRA_PATH` | `/opt/homebrew/bin:/usr/local/bin` | Directories prepended to `PATH` at the top of the script; tests only |
| `CLAUDE_VERSIONS_DIR` | `~/.local/share/claude/versions` | Claude binary versions directory; `claude-release.last` is used to detect old → new |

Key behaviours:

- **One assignment carries at most 5 versions / 80 kept+unmatched entries in total**, in JSON order; the rest wait for the
  next round (the JSON also carries `deferred_versions`). The first version is always sent, so a single oversized version
  never blocks forever. The request id is `release-triage-<kind>-<to>`; when truncated, `<to>` is the last version of this
  batch so the next batch does not collide with the same id and get swallowed by the daemon's dedup.
- **Quota gate**: before dispatching, `bin/agm state` + `bin/agm quota` locate the target bot's identity slot
  (`<kind>:<identity>`, else `<kind>`). 5h usage ≥ `AGM_TRIAGE_QUOTA_MAX` or a `limit_hit` means no dispatch, one log line,
  and the rows stay `pending` for the next round. If it cannot be determined (endpoint broken, slot not found) it
  **dispatches anyway** and logs it, rather than doing nothing forever because an endpoint is down. It only queries when
  there really is a changelog pending or a binary diff, once per round.
- **Lock**: `release-triage.lock` holds pid + time (same format as `daemon-update-kick.sh`), and
  `release-triage.lock.guard` is an OS advisory lock serializing old/new lock creation and stale recovery, which prevents
  duplicate dispatch. A dead owner (including a pid reused by another process) with no process still holding the advisory
  lock is reclaimed. A live runner within `AGM_LOCK_QUIET_SECS` is skipped silently, beyond `AGM_LOCK_HUNG_SECS` pushes
  `ops_alert` (`runner_hung`); a guard owner that cannot be verified yet holds the lock for long also alerts; a lock that
  cannot be reclaimed pushes `stale_lock`.
- **Dependencies**: the script prepends `PATH=/opt/homebrew/bin:/usr/local/bin:$PATH` itself; a missing `python3` pushes
  `ops_alert` (`missing_dependency`) and logs, never a silent `exit 0`. It depends only on `python3` and `bin/agm` (quota
  goes through `agm quota`, no `curl`/`gh`; opening issues is the daemon's job).
- **A successful dispatch is written back to the ledger**: after `assign` succeeds, it calls
  `bin/agm release-triage dispatched --kind <k> --version <v>…` for **the versions actually sent in this message** (the
  truncated batch, not everything pending). Only then do the rows leave `pending` (the next round stops returning the same
  batch, and the clock for "`dispatched` for over 6 hours without a verdict goes back to pending" starts). A failed `assign`
  marks nothing; a failed `dispatched` call logs one line and still `exit 0`s, and does **not** redispatch (the request id
  blocks a duplicate assignment).
- **`publish` retry**: every round (whatever is pending, quota gate or not) calls `bin/agm release-triage publish --kind <k>`
  once for each kind, giving versions stuck at `judged` because `gh` failed a retry path (the daemon has no timer for it).
  It is silent when there is nothing to retry (empty results, `disabled`, `deferred`); only `published`/`failed` are logged.
  An old `bin/agm` that does not know `release-triage` (argparse exit 2) is logged once and skipped; the state file
  `release-triage-publish-unsupported` remembers it was already said, and it clears itself after a new agm is installed.
- If `release-triage-check` fails for one kind (feed unreachable, …), only that kind's changelog dispatch is skipped — it is
  not treated as "no new version" — and the other kind still runs; if the Claude binary also changed version, the
  binary-only assignment is still sent.
- When a Claude binary diff and changelog pending coexist they go out as one assignment; `claude-release.last` only
  advances after a successful dispatch, and the `release-triage` announcement id is used. A binary-only message uses
  `agm-claude-release-<version>-notice`, so a changelog notice sent earlier with different text never shares an id.

**Division of labour with the other kicks:**

- `claude-release-kick.sh` is a compatibility wrapper kept for the existing launchd/systemd `com.agm.claude-release`
  schedule; it just `exec`s `release-triage-kick.sh`. The Claude binary diff is merged into the changelog triage and is no
  longer dispatched separately. The state file is still `claude-release.last`, updated by the unified kick once the binary
  diff has been dispatched. If `com.agm.release-triage` and `com.agm.claude-release` fire in the same second, they share
  `release-triage.lock`: the latecomer skips quietly and never overlaps the dispatch.
- `herdr-update-kick.sh` (#66): herdr is a separate pipeline; #204 deferred merging it to a second phase.
- `daemon-update-kick.sh`: lock reclaim and `ops_alert` are written the same way, for consistent formats.

launchd plist example (`~/Library/LaunchAgents/com.agm.release-triage.plist`; **it must carry `EnvironmentVariables.PATH`**,
because launchd's default PATH lacks `/opt/homebrew/bin`):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.release-triage</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/release-triage-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>1800</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

**Claude API triage (#1919).** With `[claude_api.uses.release_triage]` enabled, the kick runs `agm release-triage auto`
before the quota gate: the daemon calls the API, books verified verdicts into the ledger and notifies the supervisor
directly, and versions the API judged are no longer sent to the coordinator. Versions where the API is off, has no key,
fails or fails validation are dispatched to the coordinator as before (the log only records real failures; "off" is quiet).
With a new claude binary, the kick runs `bin/claude_binary_diff.py` only when the API path is on (`auto --binary-diff-file`),
and `claude-release.last` advances only once the API folded the conclusion into the notice (`binary_used`). Tests are in
the #9 series of `release-triage-kick_test.sh`.

The Claude binary diff itself is deterministic and lives in `claude_binary_diff.py OLD NEW [--timeout SECS] [--max-lines N] [--max-bytes N]`
(#1919). It prints plain text (each block under a `== … ==` header) of: the set difference of `--help` and every
subcommand's `--help`, the set difference of `.describe("…")` fields, and the set difference of `CLAUDE(_CODE)?_*`
environment variable names — additions and removals both listed; output past `--max-lines`/`--max-bytes` ends with a
`[truncated: …]` line. The binary is scanned in 8 MiB chunks and `--help` runs as a child process (empty environment,
throwaway HOME, timeout, output cap), so a 200 MB file is never read into memory and none of it should run inside the
daemon. Exit 0 means a report was produced (a block that failed shows up as an `ERROR:` line); 2 means OLD/NEW is not an
executable. Isolated test: `bash scripts/ops/claude_binary_diff_test.sh`. Install:
`install -m 755 scripts/ops/claude_binary_diff.py ~/.config/agents-manager/supervisor/AGM/bin/`.

Isolated tests: `bash scripts/ops/release-triage-kick_test.sh` (fake `bin/agm` and `agents-managerd`; covers
`env -i PATH=/usr/bin:/bin` to imitate launchd, stale and live locks, the quota gate, and the merged binary diff
assignment), and `bash scripts/ops/claude-release-kick_test.sh` for the wrapper. `release-triage-check`'s slicing and
bucketing are pinned by the daemon's `cargo test`, not re-tested here.

Production install (**needs AGM approval, and the daemon-side `release-triage-check` must be live first** — without that
subcommand the script only logs "check failed" every round):

```sh
install -m 755 scripts/ops/release-triage-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/release-triage-task.md ~/.config/agents-manager/supervisor/AGM/
install -m 644 scripts/ops/release-issue-task.md ~/.config/agents-manager/supervisor/AGM/
install -m 644 scripts/ops/claude-release-diff-task.md ~/.config/agents-manager/supervisor/AGM/
install -m 644 scripts/ops/claude-release-task.md ~/.config/agents-manager/supervisor/AGM/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agm.release-triage.plist
```

**Before turning on `[release_triage] publish = true`** (the close condition of #204), do a dry run first: see which issues
it would open, what the bodies look like, whether dedup hits, and whether all labels exist. The dry run only reads `gh`
(`auth status` / `repo view` / `label list` / `issue list`) and **opens no issue and writes nothing to the ledger**, so it
works while `publish = false`:

```sh
~/.config/agents-manager/supervisor/AGM/bin/agm release-triage publish --dry-run          # every judged version
~/.config/agents-manager/supervisor/AGM/bin/agm release-triage publish --dry-run --kind codex --version 0.156.0
```

`checks` must all be green before an issue can actually be opened: `gh_auth_ok`, `repo_ok`, `can_write`
(`viewer_permission` is `ADMIN`/`MAINTAIN`/`WRITE`/`TRIAGE`), `issues_enabled`, and `labels_missing` empty.
`gh issue create --label` **hard-fails on a nonexistent label**, so a single missing one leaves the whole version stuck at
`judged`; create these five on the repo first: `release-triage`, `upstream:claude`, `upstream:codex`, `triage:guard`,
`triage:adopt`. Each proposal's `action` is `create` (will open) | `comment` (`duplicate_of`, comment only) | `existing`
(the remote already has the same marker, closed ones included; will not reopen) | `already_logged` |
`skipped_version_limit` | `deferred_daily_limit` | `remote_unknown` (the `gh` check failed so dedup cannot be answered).
Once `publish = true`, the kick's per-round `publish` retry opens every `judged` version (at most 4 issues per version,
8 per 24 hours), so confirm the `would_create` numbers are what you expect before turning it on.

## herdr-update-kick.sh

When herdr ships a new version, summarize "is it useful to us, could it break us" and hand it to AGM's schedule (issue #66,
SPEC §18.2b). Read-only and dispatch-only: it **neither upgrades nor restarts the herdr server** — a real upgrade is always
done by hand in maintenance mode (§6.5.2) after AGM approves. Version comparison and CHANGELOG slicing are delegated to
`agents-managerd herdr-update-check` (`crates/am-base/src/herdr_update.rs`); the script only asks for the local version,
asks GitHub for the latest stable version, fetches the full CHANGELOG, and follows the returned JSON to decide whether to
dispatch.

| Variable | Default | Meaning |
| --- | --- | --- |
| `AGM_DIR` | `~/.config/agents-manager/supervisor/AGM` | Supervisor working directory (`bin/agm`, logs, state) |
| `AGM_REPO` | `~/project/agents-manager` | Repo to find `target/release/agents-managerd` in (fully overridden by `AM_BINARY`) |
| `AM_BINARY` | `$AGM_REPO/target/release/agents-managerd` | Binary used to call `herdr-update-check` |
| `HERDR_REPO` | `herdrdev/herdr` | GitHub repo for the latest stable version and the CHANGELOG |
| `HERDR_CHANGELOG_URL` | `https://raw.githubusercontent.com/<HERDR_REPO>/master/CHANGELOG.md` | Source of the full CHANGELOG (a `file://` URL works for tests) |
| `AGM_HERDR_UPDATE_BOT` | (none) | Whom to dispatch to; if unset, falls back to `herdr_update_bot_id` → `release_bot_id` → `responder_bot_id` in `runtime.json` |
| `AGM_EXTRA_PATH` | `~/.local/bin:/opt/homebrew/bin:/usr/local/bin` | Directories prepended to `PATH` (order follows `which herdr` in a login shell); empty string = prepend nothing; tests only |
| `AGM_LOCK_STALE_SECS` | `120` | With no discoverable owner (including locks left by older script versions that have no owner file), a lock older than this is reclaimed |
| `AGM_LOCK_HUNG_SECS` | `3600` | Owner alive but stuck this long: push `ops_alert` (`runner_hung`), do not steal the lock |
| `AGM_FAIL_ALERT_AFTER` | `2` | Consecutive rounds that could not finish the check before `ops_alert` (`check_failing`) |

State file `herdr-update.last` is the version already dispatched; written only after a successful dispatch, and the same
version is never dispatched twice. The script prepends PATH itself (launchd's default lacks Homebrew, where herdr and gh
usually live); a missing `herdr`/`gh`/`python3`/`curl` pushes `ops_alert` (`missing_dependency`) and logs, never a silent
`exit 0`.

**Lock** (from the #66 review): `herdr-update.lock` holds pid + time (same as `release-triage-kick.sh`). If SIGKILL or a
power cut skipped the EXIT trap and left the lock on disk, the next round reclaims it when the owner is gone (including a
reused pid) and logs one line; an old-style plain `mkdir` lock (no owner file) older than `AGM_LOCK_STALE_SECS` is reclaimed
the same way, a just-created one is left alone; an owner that is alive but stuck past `AGM_LOCK_HUNG_SECS` pushes
`runner_hung`, and one that cannot be reclaimed pushes `stale_lock`.

**Consecutive failures must be visible** (from the #66 review): every "this round could not finish the check" exit — local
version unreadable, latest version unreachable, CHANGELOG unreachable, version comparison failed, comparison report
unintelligible (not JSON or no boolean `should_notify`; #226 — never treated as "no new version"), nobody to dispatch to,
dispatch failed, binary missing — is counted in `herdr-update.fails` in addition to the log. After `AGM_FAIL_ALERT_AFTER`
consecutive rounds (one round a day, so "still broken the next day") it pushes `ops_alert` (`check_failing`), so one network
blip makes no noise. A fully completed check (including "no new version") or a successful dispatch resets the count.

Isolated test: `bash scripts/ops/herdr-update-kick_test.sh` (fake `herdr`, `gh`, `agents-managerd`, `bin/agm`; `file://`
CHANGELOG; covers `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin` imitating launchd, missing dependencies raising an alert,
stale and live locks, repeated failures raising an alert; also runs under the system `/bin/bash` 3.2). The correctness of
`herdr-update-check`'s version comparison and CHANGELOG slicing is pinned by `cargo test -p agents-managerd`
(`crates/am-base/src/changelog.rs`, `crates/am-base/src/herdr_update.rs`) and not re-tested in this script's test.

Install (**needs AGM approval**; do not `launchctl bootstrap` on your own):

```sh
install -m 755 scripts/ops/herdr-update-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 755 scripts/ops/herdr-lan-check.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

The full manual steps after an upgrade is approved are in [`herdr-upgrade-runbook.md`](herdr-upgrade-runbook.md).
`herdr-lan-check.sh` must run from a herdr pane with **the path of the real binary as its first argument**
(`/opt/homebrew/bin/herdr`): with no argument it skips the per-bot shims on PATH
(`~/.config/agents-manager/bots/*/bin/herdr`, unsigned) and shell scripts and resolves symlinks, but if only a shim is on
PATH it FAILs explicitly. It looks up the new binary's `codesign -dv` identifier in
`/Library/Preferences/com.apple.networkextension.plist`, then connects over the LAN with node; Apple's built-in `nc`,
python3 and curl cannot stand in. On any failure or a missing node, stop and ask the user to grant "Local Network"
permission; do not retry by restarting.

launchd plist example (`~/Library/LaunchAgents/com.agm.herdr-update.plist`; **it must carry `EnvironmentVariables.PATH`**:
launchd's default PATH lacks `/opt/homebrew/bin`, where herdr and gh live, and herdr may also be in `~/.local/bin`; the
script prepends it once more itself):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.herdr-update</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/herdr-update-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>86400</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/Users/USER/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

**The compatibility sandbox (issue #66, approach §3) was not built.** The reasoning and cost analysis are in the last
paragraph of SPEC §18.2b: a sandbox either checks only protocol shape and misses real behavioural differences, or needs a
near-production environment that is expensive to rebuild; it is left for AGM to decide after it receives the dispatch and
has seen what the release actually changed.

## ci-watch-kick.sh

Watchdog for main's CI (issue #211). The rule used to require only a local `check.sh` and nobody looked at GitHub's result,
so from 2026-09-16 main's CI stayed red for days unnoticed. Since issue #716 the default source is the commit status
`ubuntu-ci` (`AGM_CI_SOURCE=ubuntu-ci`; failure logs are read from the local `AGM_CI_LOG_DIR` if present, otherwise fetched
over ssh from `AGM_CI_HOST`); the `gh run …` behaviour described below is `AGM_CI_SOURCE=actions`, the old mode. It runs
every 10 minutes under `com.agm.ci-watch`, only opens issues, comments and dispatches, and **does not change code, restart
anything or close issues**. It is shaped like `release-triage-kick.sh` (lock with stale recovery, PATH prepended at the top,
missing dependencies go through `ops-alert`, silent when nothing happens).

- Each round it lists the last 20 main runs (`gh run list --branch main --workflow CI -L 20`) and looks only at **completed**
  ones: success is green, failure/timed_out/startup_failure is red, cancelled/skipped/in-progress are treated as unseen.
  In `ubuntu-ci` mode the last 20 main commits' `ubuntu-ci` statuses are normalized into the same shape; a commit with no
  `ubuntu-ci` status (a sha skipped by merging) counts as unseen and pending is not yet complete. If the latest result is
  `error` for `AGM_ERROR_ALERT_AFTER` (default 3) consecutive rounds, it is an infrastructure problem (disk full,
  repeated interruption) rather than a test failure, and it alerts `ci_infra_error` instead of opening a "red" issue.
- The state file `ci-watch.state.json` records the current red stretch: `first_red_sha`, `first_red_run`, `issue`,
  `assigned`, `failures`.
  - **Green → red**: extract the failing test names from the failed log (cargo `test X ... FAILED` / the `failures:` block,
    Python `ERROR:`/`FAIL:`), open one issue (labelled `ci-red`, created if absent; the body holds the first red run, the
    failure list and the `git log` between the last green and the first red), then dispatch with
    `bin/agm assign … --request-id ci-red-<first_red_sha>`. Before opening, it checks for an already-open `ci-red` issue (and
    adopts it if the state file was lost, rather than reopening or redispatching).
  - **Still red**: no new issue, no redispatch; it comments once on the same issue only when the failure list gains a new
    test. A dispatch that failed last round is retried (same request id, deduplicated by the daemon).
  - **Red → green**: comment "green again since <sha>, run <id>" on the issue and clear the state; it does **not** close the
    issue — whoever fixed it should.
- If `gh` fails or is rate-limited, the round does nothing, leaves the state alone and logs one line — it never reports a
  false red or green.
- Whom to dispatch to: `AGM_CI_BOT` > `ci_bot_id` in `runtime.json` > `responder_bot_id`; never the supervisor itself. The
  task body is `ci-watch-task.md`.
- If a red stretch ends green without anyone closing its issue and the next red stretch arrives, the still-open issue is
  adopted — so the fixer must remember to close it.

launchd plist example (`~/Library/LaunchAgents/com.agm.ci-watch.plist`; **it must carry `EnvironmentVariables.PATH`**,
launchd's default PATH lacks `/opt/homebrew/bin`, where `gh` lives):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.agm.ci-watch</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>/Users/USER/.config/agents-manager/supervisor/AGM/bin/ci-watch-kick.sh</string>
  </array>
  <key>StartInterval</key><integer>600</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
```

Isolated test: `bash scripts/ops/ci-watch-kick_test.sh` (fake `gh` and `bin/agm`; covers `env -i` with a minimal PATH, stale
and live locks, `gh` failures, and a lost state file). It never calls the real `gh issue create`.

Production install (**needs AGM approval**):

```sh
install -m 755 scripts/ops/ci-watch-kick.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/ci-watch-task.md ~/.config/agents-manager/supervisor/AGM/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agm.ci-watch.plist
```

## What a lease does and does not cover

A lease constrains **the path through the API and these scripts**:

- These scripts, `bin/agm`, and rebuild/restart requests via `/api/supervisor/*`.
- While a `restart` lease is held, the daemon holds assignment dispatch (no work is dropped; it is delivered after the
  window ends).
- **Only supervisor assignment dispatch is gated**: `POST /api/bots/{id}/prompt` is not. The user typing, or a PM handing
  off the next task, still gets through during a window.

What it cannot cover:

- Any shell on the machine directly `kill`ing the daemon, running `cargo build --release` on its own, or replacing the
  binary some other way. No OS-level lock can be enforced from the daemon; **this is a known boundary — do not pretend
  otherwise in docs or reports.**
- So the rule still stands: ask AGM before a restart or release rebuild (see the repo's `AGENTS.md`). The lease makes
  "I asked" stay true for the duration of the action; it does not replace asking.

The safety API and the embedded CLI update require rebuilding and deploying the daemon, and then AGM deploying the kick.
Until then, keep the production client-side filter hotfix; this repo version no longer excludes on its own, and an old
endpoint makes this version skip dispatch.

## outbox-gc.sh, pane-gc.sh, browser-gc-kick.sh, browser_gc_linux.py (issue #318)

Purely mechanical cleanup scripts that used to exist only in AGM's `bin/`, with no version control and no tests, yet are
all destructive (delete files, kill processes, close panes). The repo is now the source, and the installed copies are
byte-identical (`cmp`); `browser-gc-task.md` is the body of the browser-gc bot's assignment.

- `outbox-gc.sh`: runs every 10 minutes (`com.agm.outbox-gc`). Deletes files under the outbox (bot subdirectories included)
  whose mtime **and** ctime are both over 60 minutes old (a file that was `mv`d/`cp -p`d in counts from when it arrived),
  and empty directories. Directories of sharing bots (SPEC §20.5) use the **share retention policy** (#850) instead; which
  bots are sharing is decided by the share list in the daemon database (`shared_bots`, #1291), not by `.am-share-keep`:
  keep for 14 days (deleted only when both mtime and ctime exceed it), at most 500 MiB / 1000 files per bot, deleting the
  oldest by mtime first beyond that; the marker file itself is not deleted (`OUTBOX_SHARE_KEEP_DAYS`,
  `OUTBOX_SHARE_CAP_BYTES`, `OUTBOX_SHARE_CAP_FILES` are test seams, as is the clock `OUTBOX_GC_NOW`). Guardrails: it refuses
  an `AM_OUTBOX_ROOT` outside `~/.config/agents-manager/outbox*`, the root and its parents may not be symlinks, and cleanup
  first enters a directory and then works with relative paths.
  - Dependency: `sqlite3` (reads the share list read-only). If the list cannot be read (DB missing, `sqlite3` failing or
    absent, locked for over 5 s) it deletes nothing that round, logs "refusing the whole cleanup: …" to `outbox-gc.log` and
    exits 1; if outbox cleanup seems to have stopped, look at that log first.
- `pane-gc.sh`: closes `claude auth login` / `gcloud auth login` / `codex login` panes stuck for over 24 hours; phantom
  panes are only logged. Called by `browser-gc-kick.sh` (and by `browser_gc_linux.py` on Linux).
- `browser-gc-kick.sh` (macOS): `com.agm.browser-gc`, **`StartInterval 1800` (30 minutes)**. Reaps orphaned headless Chrome
  processes with no CDP connection that are over 2 minutes old, deletes unused `/tmp/am-*` Chrome profiles, runs
  `pane-gc.sh`, then dispatches `browser-gc-task.md`. Has a lock with stale recovery (issue #490). (This once said "every 6
  hours", but the real machine always ran 1800 s — the **documentation was wrong**, not the schedule; issue #487 pinned the
  plist to the real value when it entered version control.)
- `browser_gc_linux.py` (Linux): `com.agm.browser-gc` every 30 minutes. Reclaims this user's headless Chrome that is orphaned
  (parent is pid 1 or the user's own `systemd --user`), over 2 minutes old and has no CDP connection, cleans old
  safely-marked `/tmp/am-*` profiles, then runs `pane-gc`. If `ss` is missing or CDP state cannot be determined, it keeps
  the process. The unit sets `HERDR_SESSION=agents-manager` (consistent with daemon-update) so the trailing `pane-gc.sh` can
  reach the herdr server the daemon uses; when `herdr pane list` returns non-JSON or an error JSON, pane-gc logs one reason
  line to `browser-gc.log` and stops, with no traceback. It does not start a browser-gc bot or dispatch an ego-browser task.

Isolated tests: `bash scripts/ops/outbox-gc_test.sh`, `pane-gc_test.sh`, `browser-gc-kick_test.sh`,
`browser-gc-linux_test.sh` (fake `ps`/`lsof`/`herdr`/`bin/agm`, `kill` replaced by a function, `/tmp/am-*` replaced by a
temp directory; they never kill processes, close panes or touch the real outbox or HOME). Install like the other kicks:
`install -m 755 scripts/ops/{outbox-gc,pane-gc,browser-gc-kick}.sh ~/.config/agents-manager/supervisor/AGM/bin/` and
`install -m 644 scripts/ops/browser-gc-task.md ~/.config/agents-manager/supervisor/AGM/` on macOS; on Linux install
`browser_gc_linux.py` and `pane-gc.sh` instead of the kick, and the systemd units go through the supervisor.

## disk-gc.sh (issue #835, Linux)

Reclaims build caches and raises an alert when disk usage is high. Nothing used to reclaim disk on a schedule: the daemon's
`select_gc` only runs when `[build.remote] enabled`, and each worktree's `target/` kept piling up.
`com.agm.disk-gc.timer` (`OnActiveSec=600s`, `OnUnitActiveSec=3600s`, hourly) runs `bin/disk-gc.sh`; systemd only, **no
launchd plist** (macOS is a separate matter).

| Step | Target | Rule (overridable via env) |
| --- | --- | --- |
| (a) | Top-level directories under `~/.cache/agents-manager/remote-cargo/*` | Deleted when mtime is older than `DISK_GC_REMOTE_CARGO_DAYS` (7) days and no process has its cwd or an open file underneath; regardless of the `build.remote` setting |
| (b) | `target/*/incremental/*` of the main tree, every worktree registered in `git worktree list` (including ones like `~/project/agm-*` outside `.claude/worktrees`, plus unregistered `.claude/worktrees/*`), and the ubuntu-ci resident clone (`${AGM_CI_ROOT:-~/.cache/agents-manager/ci}/repo`) | Subdirectories with mtime older than `DISK_GC_INCREMENTAL_DAYS` (2) days are deleted |
| (c) | Each worktree's `target/` (**not the main tree, not ubuntu-ci's clone**) | If the newest `target/*/.fingerprint` (and its first level) is older than `DISK_GC_TARGET_DAYS` (7) days → delete the whole `target/`; with no `.fingerprint` at all, make no judgement. ubuntu-ci's `target/` is a warm cache and is never deleted |
| (d) | The disk holding `$HOME` | Usage ≥ `DISK_GC_ALERT_PCT` (85) or free space < `DISK_GC_ALERT_FREE_GB` (20) → `agm ops-alert --source disk-gc --reason disk_low` |

Safety: it deletes only those explicit paths; a path containing `..`, outside `$HOME`, with any symlink along the way, or
whose real location does not match abandons the whole step; after `cd` it deletes with relative paths only. A worktree with
a process whose cwd is inside, or a process holding a file open under `target/`, is left entirely alone (for the main tree
only open files under `target/` count, since someone is always working there); if `/proc` is unreadable, nothing is deleted.
Each round writes one line to `supervisor/AGM/disk-gc.log` (MB freed, what was skipped and why). `/tmp` leaks are handled by
`sweep_stale_tmp` in `ubuntu-ci.sh` and are not duplicated here.

Isolated test: `bash scripts/ops/disk-gc_test.sh` (HOME pointed at a temp directory, old files made with `touch -d`, fake
`df` and `agm`; it never touches the real caches). Install (**needs AGM approval**):

```sh
install -m 755 scripts/ops/disk-gc.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/systemd/com.agm.disk-gc.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now com.agm.disk-gc.timer
```

## remote-cargo-prewarm.sh (issue #1855, Linux)

Warms the cargo cache on the external build host (r7) so that when a child starts work it hits a warm cache instead of a
cold start (measured: a cold whole-tree check on r7 takes 3 min 10 s, a warm one-line change 4.8 s). The systemd timer
`com.agm.remote-cargo-prewarm.timer` runs it first 10 minutes after the timer starts and then every 600 seconds. Each run:

1. Exits at once if it cannot take the lock (a previous round is still running — at most one at a time).
2. Fetches `origin main`; if the sha equals the one in the state file, exits (a successful sha is warmed once).
3. Uses a dedicated worktree (`AGM_PREWARM_WORKTREE`) that the script must have created itself (a marker in the gitdir,
   the same git common dir, detached, clean). Otherwise it refuses, logs, and **does not touch it** (an existing worktree
   of the same name may hold someone's WIP).
4. Calls the daemon's `remote-cargo` helper step by step, with the same target set as `scripts/check.sh`'s daemon/xtask/crates
   tests (desktop excluded: its build script needs a sidecar that only the packaging flow supplies); every step is
   `-p <package>`, with the same feature unification as `check.sh`.
5. **Runs only on r7 and never falls back to local**: if the helper returns 125 (not enabled / unreachable), 75 (r7 slots
   full) or 126 (misconfiguration), it skips without counting an attempt and tries again next round.
6. Success records the sha in `STATE`. A transient failure (124 timeout, or an interruption by a signal ≥128) goes to
   `RETRY`: at most 3 attempts per sha, with one round of backoff between the 2nd and 3rd; once used up it gives up on this
   sha and waits for the next main. Any other cargo failure (a compile error, say) is treated as a problem with this sha and
   abandoned without retry.

Cache key: the remote directory hash is seeded with the git common dir (`remote_cargo.rs` `cache_hash`, #1856), so the
prewarm worktree and the children's worktrees of the same repo map to the same `shared/` directory, which is why the
warmed cache is usable by them. See the external-cargo section of `docs/SPEC.md`. Overridable for tests:
`AGM_PREWARM_REPO`, `AGM_PREWARM_WORKTREE`, `AGM_PREWARM_CONFIG`, `AGM_PREWARM_DAEMON`, `AGM_PREWARM_STATE`,
`AGM_PREWARM_FETCH_TIMEOUT`. Isolated test: `bash scripts/ops/remote-cargo-prewarm_test.sh`. Install (**needs AGM approval**):

```sh
install -m 755 scripts/ops/remote-cargo-prewarm.sh ~/.config/agents-manager/supervisor/AGM/bin/
install -m 644 scripts/ops/systemd/com.agm.remote-cargo-prewarm.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now com.agm.remote-cargo-prewarm.timer
```

## dev-server-kick.ts (issue #418)

Watchdog for the port-5173 dev server. **Before 2026-09-24 it existed only in `AGM/bin/`**: `docs/SPEC.md` §18.1 describes
the behaviour in detail, but the code had no version control, no review and no tests, and it `kill`s the process holding the
port. The repo is now the source.

- launchd `com.agm.dev-server` (Linux: the systemd user timer of the same name): `StartInterval 60`, `RunAtLoad`, running
  `bun run …/AGM/bin/dev-server-kick.ts`.
- The 5173 worktree is `${AGM_REPO:-~/project/agents-manager}-main` (`AGM_DEV_REPO` overrides it entirely). The listener
  is found with `lsof` on macOS and `ss` on Linux (#676).
- Behaviour and decision order are in `docs/SPEC.md` §18.1 (healthy = bound to `*`/`0.0.0.0` and a local curl answers; only
  orphaned vite is reaped; if node or `vite.js` cannot be found it prefers to skip this round rather than let bun run vite
  in its place).
- **The watchdog runs on bun; vite always runs on node**: bun's upgrade socket has no `destroySoon`, so vite crashes when
  the daemon restarts and the proxy connection drops.
- **Alerts (issue #859).** When the worktree is missing, node cannot be found, or `vite.js` cannot be found, besides writing
  `dev-server.log` it runs `agm --compact ops-alert --source dev-server --reason dev_worktree_missing|dev_node_missing|dev_vite_missing`
  (the daemon sends one per hour per source+reason; `AGM_BIN` overrides the agm path). These three used to log only, so
  nobody knew when 5173 was down.
- **Host setup (once).** The worktree 5173 serves from must exist first, and when handing over you must remove any
  temporary unit (a transient unit such as `am-vite-5173` has ppid ≠ 1, so the watchdog will not reap it):

```sh
git -C ~/project/agents-manager worktree add --detach ~/project/agents-manager-main origin/main \
  && (cd ~/project/agents-manager-main/web && bun install --frozen-lockfile)
systemctl --user stop am-vite-5173   # if such a transient unit exists; the next watchdog round starts vite on node bound to 0.0.0.0
```

Isolated test: `bash scripts/ops/dev-server-kick_test.sh` (fake `lsof`/`ss`/`ps`/`git`/`bun`/`node`; the copy's `PORT` is
changed to a test port, the fake `lsof`/`ss` report only pids the test itself spawned, and the whole scenario set runs once
with `lsof` and once with `ss`; **it never touches the real 5173 or a real vite**). It skips itself without bun/python3.

Install (**needs AGM approval**):

```sh
install -m 755 scripts/ops/dev-server-kick.ts ~/.config/agents-manager/supervisor/AGM/bin/
```

## herdr-full-restart.sh (issue #418)

Full herdr restart: boot out the two herdr launchd jobs → kill every herdr server → clear the sockets → bootstrap the jobs
back → bring the default server back. Written on 2026-09-22 when the user ordered a machine-wide restart, and likewise
originally only in `AGM/bin/`. **No launchd schedule**; run by hand.

- Order is the point: **bootout first, then kill the servers** — the other way round, launchd would pull the servers right
  back.
- **Starting the default server is the only step with no self-healing path.** Among the killed sessions, `agents-manager` and
  `am-attach-remote` have launchd jobs that bootstrap back, and other named sessions are restarted by the daemon's
  `ensure_session` (`daemon/src/state.rs`) the next time they are needed; **`default` alone is not** — `ensure_session`
  explicitly refuses to start the user's own session on their behalf. So if that step fails, someone must be told.
- Fixed in issue #455 (2026-09-24): the script used `nohup setsid …`, but macOS has no `setsid` (it is util-linux), so
  `nohup` failed to find it and herdr was never executed; worse, the line was written `cd … && nohup … &`, where `&` binds
  the whole `&&` list, so `$!` was the subshell's pid and had a value **whether or not the server started** — hence a failure
  was logged as "default server started pid=…". The 2026-09-22 log showing `default stopped` in `session list` was exactly
  this. Now `setsid` is not used (`nohup` + `&` + `disown` is enough, same style as `ensure_session`), `$!` is the server
  itself, and `default server up` is logged only after `herdr session list` shows `default running`; if that never
  happens it writes FAIL and exits rc=1.
- Servers are no longer killed with `pkill -f 'herdr.*server'`: that pattern also killed claude/codex bots whose argv (a
  long persona) happened to mention herdr/server, plus ssh and `herdr pane list server`. It now identifies them from
  `ps -axo pid=,args=`: the first word's basename is `herdr` and the first positional argument after skipping flags
  (`--session`/`--machine` with their values) is `server`; only those pids get TERM, and any still alive after 5 s get KILL
  (shared by the macOS and Linux sections).
- Paths and uid (`gui/501`) are hard-coded to the dev machine — existing behaviour kept when the script entered the repo.
- **Linux host** (issue #677): follows the section at the top of the file. herdr servers are supervised by the systemd user
  unit `herdr@<session>.service` (`scripts/ops/systemd/herdr@.service`, installed and compared by `ops-sync` together with
  the `com.agm.*` units). It records the `herdr@*.service` units that were active → stops them → kills leftover servers →
  clears the sockets in `$HOME/.config/herdr` → starts the same set again, rc=1 if `is-active` does not come back; the
  default server is started only if it was running before. Every path is derived from `$HOME`, and without
  `XDG_RUNTIME_DIR` it fills in `/run/user/<uid>`.
- `herdr-upgrade.sh` is macOS-only (it swaps the binary through brew and rolls back via the keg); on Linux it exits 2
  immediately and changes nothing.

Isolated test: `bash scripts/ops/herdr-full-restart_test.sh` (`launchctl`, `systemctl`, `kill`, `ps`, `sleep`, `herdr` and
`uname` are all intercepted with **injected shell functions**, not PATH; sockets and plists live in a temp directory; the
macOS and Linux sections each have their own scenarios).

Install (**needs AGM approval**):

```sh
install -m 755 scripts/ops/herdr-full-restart.sh ~/.config/agents-manager/supervisor/AGM/bin/
```

## project-transfer (issue #710)

Hands a single project over from one daemon to another (e.g. Mac → agm-host, taken over by the remote host `m4p`). Not
installed; run it from a checkout. The flow, rewrite rules and rejection conditions are in SPEC §11.9. `export` reads the
source DB read-only (it takes a backup snapshot first and deletes it after reading). `import` needs the **target daemon
stopped** (it takes the same `daemon.lock`); run `--dry-run` first to see the summary. For a project that will run on the
target machine locally, use `--host local --path-map /source=/target`, and move the native conversations first with
transcript-transfer below (SPEC §11.9a). For the coordinator's (AGM's) own data, add `--with-supervisor` to both export and
import (merge rules in SPEC §11.9b); group tasks always travel with the project. Isolated test:
`scripts/ops/project-transfer_test.sh` (fake DB; the schema is extracted from `crates/am-base/src/db.rs`).

## transcript-transfer (issue #717)

When a project runs locally on the receiving machine, moves the bots' native conversation files (claude/codex/grok) there and
rewrites their paths so that `start?resume=native` can pick them up. Run it on the source machine after
`project-transfer export`; it outputs a new bundle with `runs.transcript_path` rewritten, for
`import --host local --path-map …`. Not installed; run it from a checkout. Measured results and the runbook are in SPEC
§11.9a. Use `--dry-run` first; exit 2 means something was not found, refused, or conflicted. Isolated test:
`scripts/ops/transcript-transfer_test.sh` (a fake source $HOME and target directory; no ssh).

## cutover-to-host.sh (issue #721)

Cuts the whole `Agents Manager` and `AGM-DM-GRUP` setup over from the Mac to running locally on agm-host (#675). Run it from
a checkout on the Mac; not installed. Steps, gates, rollback and rehearsal results are in SPEC §11.9c. `cutover` is a
dry-run by default (read-only checks plus a list of what it would do); `--execute` does the work and detaches itself into
the background (log at `~/.config/agents-manager/cutover/<time>/run.log`); `rollback --state-dir <the same one>` restores;
`drill` rehearses on the target in a separate directory with a DB copy and deletes it as soon as it finishes. The helper
`cutover-helper.py` runs on both sides. `host-state-transfer.py` moves the full non-project config, the UI token, the outbox
and the identity-config directory list along with the cutover; the list records only paths and existence, and credentials
are not moved. Installation on the target and rollback both take `daemon.lock` and do not touch SQLite. Isolated test:
`scripts/ops/cutover-to-host_test.sh` (two fake daemons, fake ssh/rsync/launchctl, no production environment touched; it
also runs the host-state temp-directory test).

## ubuntu background CI and periodic full verification (issue #716)

Pushes to main no longer wait for GitHub Actions: a systemd user timer on agm-host runs `scripts/ops/ubuntu-ci.sh` every
minute (`OnBootSec=2min`, `OnUnitInactiveSec=60s`). It verifies only the newest main HEAD (one round at a time; shas in
between are not back-filled), picks ob/ops/web/daemon from each part's success baseline, and writes the result as the
commit status `ubuntu-ci`, `~/.cache/agents-manager/ci/status.json`, and `logs/<sha>.log`.

- If the script interrupts itself after pending was sent (a checkout failure, say), it posts an `error` status and writes
  `error` to status.json so pending is never left hanging (last-sha is unchanged, so the next round retries). Each part runs
  under `timeout -k` (`AGM_CI_KILL_AFTER`, default 60s), so a step that ignores TERM is still reaped and cannot hold the lock.
- ENOSPC partway through (the log contains `No space left on device` or check.sh's "only N free on failure", or the free
  space measured after the run is below `AGM_CI_MIN_FREE_GB`) is not this commit's fault: it records `error`
  (status.json `reason=disk_full`) without moving last-sha, shares the `interrupted-<sha>` counter with signal-killed
  interruptions, and only counts as a failure after 3 in a row (#945).
- `git clone` and `git fetch` have time limits (`AGM_CI_CLONE_TIMEOUT` default 600 s, `AGM_CI_FETCH_TIMEOUT` default 300 s;
  a half-open network will not hold the lock forever). The service sets `TimeoutStartSec=3h15m` (4 parts × 45m + 15m).
- Time per part (#1448): each part ends its log section with `==> [ubuntu-ci] <part> rc=<n> secs=<s>`, and `status.json`'s
  success/failure/interrupted records all carry `"parts":{"ob":<s>,"ops":<s>,"web":<s>,"daemon":<s>}` (daemon includes the
  small herdr-argv step; a part that did not run is `null`). `mode`, `select_reason`, `full_baseline`, `part_state` and
  `part_base` distinguish what this round ran from what it inherited.
- On taking the lock it writes `~/.cache/agents-manager/ci/running-since`; if it cannot take the lock and that file is
  older than `AGM_CI_HUNG_AFTER` seconds (default 11700), it calls `ops-alert --source ubuntu-ci --reason ci_hung` using the
  CI clone's `agm.py` (one per hour per source+reason; a failed alert is swallowed) (#946).

Per-part baselines live in `part-success/<part>` and advance only after that part really succeeded; the cumulative diff from
its last successful ancestor to HEAD is classified by `ci-changed-parts.sh`, so an intermediate failed or unverified change
cannot slip through. A missing baseline, a non-ancestor, an unknown path or a classification failure always runs all four
parts, and the very first update runs everything once to establish baselines. `full-success-sha` only advances when all four
parts succeeded in one round. A fully successful status description stays `ob+ops+web+daemon 全綠`; a partial one reads
`scoped <part+part>（沿用完整成功基準 <8hex>）`, a fully inherited one `scoped none（沿用完整成功基準 <8hex>）`, and
docs-only changes keep the existing `docs-only skip` format. A Rust-only change still builds the real web bundle from the
current HEAD for embedding, but does not rerun unrelated web unit tests.

`AGM_CI_FULL_INTERVAL` (default 86400 s) forces a full four-part run every 24 hours even if main has no new commits; 0
disables the periodic full run and an invalid value falls back to the default. A full run writes `full-attempt-at` when it
ends and `full-success-at` on success, so a failure does not cause a rerun every minute; a corrupt or future timestamp is
handled conservatively and cannot disable the periodic run for good. All these files live in the CI state directory, not in
the repo.

**After changing `ubuntu-ci.service` reinstall it by hand**: `cp` it to `~/.config/systemd/user/` and run
`systemctl --user daemon-reload` (script changes take effect on the next round after merge by themselves; the unit does not).
The runner uses the `ubuntu-ci.sh` that is **currently checked out** in the resident clone, so script changes apply
automatically on the next round.

Locally, agents wrap up with `scripts/check.sh changed`: a Rust-only change in a clean checkout with no
`web/dist/index.html` temporarily creates a rust-embed stub and removes it afterwards, so it neither installs dependencies
nor builds web. Web checks and the background full CI still build the real production bundle.

Isolated test: `bash scripts/ops/ubuntu-ci_test.sh` (a local bare repo, fake `gh`, `AGM_CI_GIT_BIN` as a fake git).
Install (on agm-host; needs `loginctl enable-linger ubuntu`):

```sh
git clone git@github.com:Eden-Sun/agents-manager.git ~/.cache/agents-manager/ci/repo
mkdir -p ~/.config/systemd/user
cp ~/.cache/agents-manager/ci/repo/scripts/ops/ci/ubuntu-ci.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now ubuntu-ci.timer
```
