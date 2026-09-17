//! The `cargo` PATH shim (issue #90): global build-scheduler admission for every managed bot's
//! `build`/`check`/`test`/`clippy`/… so agents keep typing plain `cargo …` while the daemon caps
//! host-wide rustc concurrency, instead of every bot copying a hand-run `cargo-slot.sh`.
//!
//! Installed into the **same** bin dir as the `herdr` shim (`herdr_shim::install_local`/`install_remote`
//! write to it too), so one PATH prepend covers both.

use std::path::{Path, PathBuf};

pub const SHIM_SH: &str = r##"#!/bin/sh
# agents-manager cargo build-slot shim (issue #90). Installed at the front of a managed pane's PATH,
# next to the herdr shim.
#
# POSIX sh only, no `set -e`: a shim that aborts a build because ITS OWN scheduling call failed is
# worse than one that just runs the build unscheduled (SPEC herdr_shim.rs 的同一條原則)。

# The real cargo: `$AM_REAL_CARGO` if set, else the first `cargo` on PATH that is not this directory.
am_real_cargo() {
    if [ -n "${AM_REAL_CARGO:-}" ] && [ -x "$AM_REAL_CARGO" ]; then
        printf '%s\n' "$AM_REAL_CARGO"
        return 0
    fi
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    printf '%s\n' "$PATH" | tr ':' '\n' | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            if [ -x "$_abs/cargo" ]; then
                printf '%s\n' "$_abs/cargo"
                break
            fi
        done
    }
}

# `build`/`check`/`test`/`clippy`/… multiply rustc processes; `--version`/`metadata`/`tree`/`fmt`/
# `fetch` don't compile anything and would just add latency for nothing.
am_cargo_is_heavy() {
    case "$1" in
        b | build | c | check | t | test | clippy | bench | r | run | rustc | doc | install) return 0 ;;
        *) return 1 ;;
    esac
}

# issue #104：跨平台 V1 只 offload verification。build/run 留本機，因為 Linux/x86_64 artifact
# 不能拿回 Apple Silicon macOS 當成本機 binary 用。
am_remote_cargo_eligible() {
    case "$1" in
        c | check | t | test | clippy) return 0 ;;
        *) return 1 ;;
    esac
}

# `http://127.0.0.1:$AM_PORT` — bots always have `AM_PORT`; a manual host shell defaults to the
# documented port (SPEC: daemon 在 127.0.0.1:7788)。
am_build_port() {
    printf '%s' "${AM_PORT:-7788}"
}

# 認證：bot 用自己的 hook token（pane 裡本來就有），人工 host shell 用一般 UI token（讀
# `~/.config/agents-manager/ui-token`，daemon 的預設位置）。兩個都沒有就回傳空字串，呼叫端看到空字串
# 就該直接跳過排程——沒有身分，daemon 也不可能認得這個名額。
am_build_auth_header() {
    if [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_HOOK_TOKEN:-}" ]; then
        printf 'X-AM-Bot-Token: %s' "$AM_HOOK_TOKEN"
        return 0
    fi
    _tok_file="$HOME/.config/agents-manager/ui-token"
    if [ -r "$_tok_file" ]; then
        _tok=$(cat "$_tok_file" 2>/dev/null)
        if [ -n "$_tok" ]; then
            printf 'X-AM-Token: %s' "$_tok"
            return 0
        fi
    fi
    return 1
}

# `sed` 從 JSON 回應挖一個欄位（herdr_shim.rs 同一招：不上 jq 依賴，冒號兩邊有沒有空白都認）。
am_json_field() {
    printf '%s' "$2" | tr ',' '\n' | sed -n 's/.*"'"$1"'" *: *"\{0,1\}\([^",}]*\)"\{0,1\}.*/\1/p' | head -n 1
}

am_cargo() {
    _real=$(am_real_cargo | head -n 1)
    if [ -z "$_real" ]; then
        printf 'agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n' >&2
        exit 127
    fi
    if ! am_cargo_is_heavy "${1:-}" || ! command -v curl >/dev/null 2>&1; then
        exec "$_real" "$@"
    fi
    _auth=$(am_build_auth_header) || {
        printf 'agents-manager: 沒有 bot／管理員身分，這次 cargo 不經過排程器，直接跑\n' >&2
        exec "$_real" "$@"
    }
    _port=$(am_build_port)
    _holder="${AM_AGENT_NAME:-manual}:$$"
    _bot_id="${AM_BOT_ID:-}"
    _purpose=$(printf '%s' "$*" | cut -c1-200)
    _url="http://127.0.0.1:${_port}/build-slots"

    _attempt=0
    while :; do
        _resp=$(curl -s -m 5 -X POST "${_url}/acquire" \
            -H "$_auth" \
            --data-urlencode "holder=${_holder}" \
            --data-urlencode "bot_id=${_bot_id}" \
            --data-urlencode "purpose=${_purpose}" 2>/dev/null)
        _rc=$?
        if [ "$_rc" -ne 0 ] || [ -z "$_resp" ]; then
            printf 'agents-manager: build scheduler 連不上（daemon 沒開？），這次不排程，直接跑\n' >&2
            exec "$_real" "$@"
        fi
        case "$_resp" in
            *'"granted":true'*) break ;;
            *'"granted":false'*)
                _attempt=$((_attempt + 1))
                if [ "$_attempt" = 1 ]; then
                    printf 'agents-manager: 全機的 cargo 名額滿了，等一個空出來（waiting_for_build_slot）……\n' >&2
                fi
                _retry=$(am_json_field retry_after_secs "$_resp")
                case "$_retry" in *[!0-9]* | '') _retry=5 ;; esac
                sleep "$_retry"
                continue
                ;;
            *)
                printf 'agents-manager: build scheduler 回應看不懂，這次不排程，直接跑：%s\n' "$_resp" >&2
                exec "$_real" "$@"
                ;;
        esac
    done

    _token=$(am_json_field token "$_resp")
    _jobs=$(am_json_field cargo_jobs "$_resp")
    _ttl=$(am_json_field lease_ttl_secs "$_resp")
    case "$_jobs" in *[!0-9]* | '') _jobs=2 ;; esac
    case "$_ttl" in *[!0-9]* | '') _ttl=180 ;; esac
    # 續約間隔取 TTL 的三分之一：daemon 端的 sweep 也是等好幾個間隔才收，一次沒續到不會立刻掉名額。
    _renew_every=$((_ttl / 3))
    [ "$_renew_every" -ge 5 ] || _renew_every=5

    (
        while :; do
            sleep "$_renew_every"
            curl -s -m 5 -X POST "${_url}/renew" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        done
    ) &
    _renew_pid=$!
    _release() {
        kill "$_renew_pid" 2>/dev/null
        curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        return 0
    }
    trap '_release' EXIT INT TERM

    # 外部 Cargo worker：helper 本身讀 config + 0600 secret file，pane 不會拿到 SSH 密碼。
    # 125 = 設定在 pane 啟動後被關掉／這個指令不適合 offload，退回本機 cargo；
    # 其他非 0 = 遠端驗證真的失敗，原樣回報，不能偷偷改成本機成功。
    if [ "${AM_REMOTE_CARGO_ENABLED:-}" = "1" ] \
        && am_remote_cargo_eligible "${1:-}" \
        && [ -n "${AM_DAEMON_EXE:-}" ] && [ -x "$AM_DAEMON_EXE" ] \
        && [ -n "${AM_CONFIG_PATH:-}" ] && [ -n "${AM_DATA_DIR:-}" ]; then
        "$AM_DAEMON_EXE" remote-cargo --config "$AM_CONFIG_PATH" --data-dir "$AM_DATA_DIR" --cwd "$PWD" -- "$@"
        _remote_rc=$?
        if [ "$_remote_rc" -ne 125 ]; then
            _release
            trap - EXIT INT TERM
            exit "$_remote_rc"
        fi
    fi

    CARGO_BUILD_JOBS="$_jobs" "$_real" "$@"
    _cargo_rc=$?
    _release
    trap - EXIT INT TERM
    exit "$_cargo_rc"
}

am_cargo "$@"
"##;

/// Rewritten every start, same as the herdr shim: an upgraded daemon never leaves an old one behind.
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = bot_dir.join("bin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("cargo");
    std::fs::write(&path, SHIM_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// Same ssh path as the herdr shim (SPEC §11.4).
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let dir = format!("{remote_bot_dir}/bin");
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/cargo\" <<'AM_SHIM_EOF'\n{shim}AM_SHIM_EOF\nchmod +x \"$D/cargo\"\nprintf 'AM_SHIM_INSTALLED\\n'\n",
        dir = crate::hosts::sh_quote(&dir),
        shim = SHIM_SH,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_SHIM_INSTALLED") {
        anyhow::bail!("remote cargo shim install did not confirm:\n{}", out.trim());
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    //! Runs the real script against a fake `cargo`/`curl`, mirroring `herdr_shim.rs`'s Sandbox.
    use std::io::Write as _;
    use std::process::Command;

    struct Sandbox {
        dir: std::path::PathBuf,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("am-cargo-shim-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            super::install_local(&dir).unwrap();
            let fake = dir.join("real");
            std::fs::create_dir_all(&fake).unwrap();
            let mut f = std::fs::File::create(fake.join("cargo")).unwrap();
            // Echoes argv and the env vars a test cares about, one per line, so assertions don't need a
            // real compiler. `$AM_TEST_FAKE_CARGO_LOG` records that the real cargo actually ran.
            f.write_all(
                b"#!/bin/sh\n\
                  { printf 'CARGO_BUILD_JOBS=%s\\n' \"${CARGO_BUILD_JOBS:-}\"; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; } \
                    >> \"${AM_TEST_FAKE_CARGO_LOG:-/dev/null}\"\n\
                  exit \"${AM_TEST_FAKE_CARGO_EXIT:-0}\"\n",
            )
            .unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(fake.join("cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Sandbox { dir }
        }

        /// `AM_TEST_CURL_SCRIPT` is the fake curl's own body (appended after the shebang); tests write
        /// whatever behavior they need (record calls, answer with canned JSON, fail to simulate no daemon).
        fn install_fake_curl(&self, body: &str) {
            let path = self.dir.join("real").join("curl");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
            let mut cmd = Command::new(self.dir.join("bin/cargo"));
            let path = format!(
                "{}:{}:/usr/bin:/bin",
                self.dir.join("bin").display(),
                self.dir.join("real").display()
            );
            cmd.env("PATH", path).args(args);
            for key in ["AM_BOT_ID", "AM_HOOK_TOKEN", "AM_PORT", "AM_AGENT_NAME", "HOME"] {
                cmd.env_remove(key);
            }
            // 假的 $HOME：不能真的去讀開發機自己的 ui-token（會讓測試偷偷通過或偷偷失敗）。
            cmd.env("HOME", self.dir.join("fake-home"));
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
        }
    }

    /// 不吃 rustc 的子指令（`--version`、`metadata`）直接 exec 真的 cargo，完全不碰排程器（curl 都不叫）。
    #[test]
    fn a_light_subcommand_skips_the_scheduler_entirely() {
        let s = Sandbox::new();
        // 沒有安裝假 curl：如果 shim 誤判去呼叫排程器，這裡會因為找不到 curl 或連不上而看得出來。
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(&[("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())], &["--version"]);
        assert_eq!(rc, 0, "{err}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("--version"), "{logged}");
        assert!(!err.contains("scheduler"), "{err}");
    }

    /// 沒有 bot token 也沒有 UI token 檔：直接跳過排程，不假裝有身分（issue #90 的「明講的 bypass」）。
    #[test]
    fn no_identity_available_bypasses_with_a_warning() {
        let s = Sandbox::new();
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(&[("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())], &["build"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("不經過排程器"), "{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("build"));
    }

    /// daemon 連不上（curl 失敗）：不排程，直接跑，不是掛在那裡等。
    #[test]
    fn an_unreachable_daemon_bypasses_instead_of_hanging() {
        let s = Sandbox::new();
        s.install_fake_curl("exit 7\n"); // curl 的「連不上」退出碼
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())],
            &["test", "-p", "agents-managerd"],
        );
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("連不上"), "{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("agents-managerd"));
    }

    /// 名額滿了先等，daemon 說 granted 才跑；granted 帶的 `cargo_jobs` 要真的傳進 CARGO_BUILD_JOBS。
    #[test]
    fn it_waits_out_a_full_scheduler_then_runs_with_the_granted_job_count() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
n=$(grep -c acquire '{log}' 2>/dev/null || echo 0)
case "$*" in
  *acquire*)
    if [ "$n" -le 1 ]; then printf '{{"granted":false,"active":2,"retry_after_secs":0}}'; else printf '{{"granted":true,"token":"tok-abc","cargo_jobs":3,"lease_ttl_secs":30}}'; fi
    ;;
  *renew*|*release*) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap())],
            &["check", "-p", "agents-managerd"],
        );
        assert_eq!(rc, 0, "{err}");
        let logged = std::fs::read_to_string(&cargo_log).unwrap();
        assert!(logged.contains("CARGO_BUILD_JOBS=3"), "{logged}");
        assert!(logged.contains("agents-managerd"), "{logged}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert!(calls.matches("acquire").count() >= 2, "第一次滿了，第二次才拿到：{calls}");
        assert!(calls.contains("release"), "結束要放：{calls}");
    }

    /// 真的 cargo 跑失敗：shim 仍然要放掉名額（不能因為建置失敗就卡住別人），並把 cargo 的結束碼原樣帶出去。
    #[test]
    fn a_failing_build_still_releases_its_slot_and_keeps_the_exit_code() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-xyz","cargo_jobs":2,"lease_ttl_secs":30}}' ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let (_, _, rc) = s.run(
            &[
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap()),
                ("AM_TEST_FAKE_CARGO_EXIT", "101"),
            ],
            &["build"],
        );
        assert_eq!(rc, 101);
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert!(calls.contains("release"), "失敗也要放：{calls}");
    }
}
