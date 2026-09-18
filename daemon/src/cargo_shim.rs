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
#
# AM_SHIM_MARKER: 這行讓 shim 認得出「PATH 上那個 cargo 其實是我自己」（am_is_shim）。不要刪。

# 這個檔案是不是這支 shim 的另一份拷貝。`AM_SHIM_MARKER` 只出現在 shim 自己的檔頭。
am_is_shim() {
    head -n 12 "$1" 2>/dev/null | grep -q 'AM_SHIM_MARKER' 2>/dev/null
}

# 這個目錄是不是某顆 bot 的 shim 目錄（`…/bots/<id>/bin`，見 daemon 的 `shim_path`）。
#
# 檔頭認不出來時的第二道：**舊版的 shim 沒有 `AM_SHIM_MARKER`**。換版期間 PATH 上同時有新舊兩份
# 是常態（shim 是 bot 啟動時才寫的），舊的那份對新的 shim 來說就是「一個普通的 cargo」——2026-09-18
# 18:40 那次就是這樣，一條呼叫鏈上 5 個 shim 互等名額。整個目錄跳掉就不必看內容。
am_is_bot_bin_dir() {
    case "${1%/}" in
        */bots/*/bin) return 0 ;;
        *) return 1 ;;
    esac
}

# The real cargo: `$AM_REAL_CARGO` if set, else the first `cargo` on PATH that is not a copy of this
# shim.
#
# **不能只跳過自己那個目錄**：一顆 bot 的 pane 會繼承祖先 pane 的 PATH，同一條 PATH 上常常掛著
# 好幾顆 bot 的 `bots/<id>/bin`（2026-09-18 實測有 6 個）。只比對自己的目錄時，下一個目錄裡的
# 「cargo」就是同一支 shim，於是 shim → shim → shim 一層一層都去拿名額：`max_concurrent=2` 被自己
# 的外層佔滿，最內層那個永遠等不到，整台機器明明是空的卻卡死（w168:p91 卡了 17 分鐘）。
am_real_cargo() {
    if [ -n "${AM_REAL_CARGO:-}" ] && [ -x "$AM_REAL_CARGO" ] && ! am_is_shim "$AM_REAL_CARGO" \
        && ! am_is_bot_bin_dir "$(dirname "$AM_REAL_CARGO")"; then
        printf '%s\n' "$AM_REAL_CARGO"
        return 0
    fi
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    printf '%s\n' "$PATH" | tr ':' '\n' | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            am_is_bot_bin_dir "$_abs" && continue
            if [ -x "$_abs/cargo" ] && ! am_is_shim "$_abs/cargo"; then
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
    # 遞迴保險絲：萬一 `am_is_shim` 認不出某一份 shim（被改過檔頭、或別的專案裝了同名 wrapper），
    # 兩份 shim 會互相把對方當成真 cargo 一路 fork 下去。寧可大聲失敗，也不要 fork 到機器躺平。
    AM_SHIM_DEPTH=$((${AM_SHIM_DEPTH:-0} + 1))
    export AM_SHIM_DEPTH
    if [ "$AM_SHIM_DEPTH" -gt 4 ]; then
        printf 'agents-manager: cargo shim 遞迴 %s 層——PATH 上有多份 shim 而且認不出來。把真 cargo 放進 AM_REAL_CARGO 再跑一次。\n' "$AM_SHIM_DEPTH" >&2
        exit 127
    fi
    _real=$(am_real_cargo | head -n 1)
    if [ -z "$_real" ]; then
        printf 'agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n' >&2
        exit 127
    fi
    if ! am_cargo_is_heavy "${1:-}" || ! command -v curl >/dev/null 2>&1; then
        exec "$_real" "$@"
    fi
    # 這條進程鏈上已經有人拿著名額（build script 或 xtask 再叫一次 cargo）：直接跑，不要再排一次。
    # 內層等的名額只會等到外層結束才空出來，而外層在等內層——就是上面那個死結。
    if [ -n "${AM_BUILD_SLOT_HELD:-}" ]; then
        AM_REAL_CARGO="$_real"
        export AM_REAL_CARGO
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
    if am_remote_cargo_eligible "${1:-}" \
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

    AM_BUILD_SLOT_HELD=1 AM_REAL_CARGO="$_real" CARGO_BUILD_JOBS="$_jobs" "$_real" "$@"
    _cargo_rc=$?
    _release
    trap - EXIT INT TERM
    exit "$_cargo_rc"
}

am_cargo "$@"
"##;

/// Rewritten every start, same as the herdr shim: an upgraded daemon never leaves an old one behind.
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::shim_refresh::bin_dir(bot_dir);
    std::fs::create_dir_all(&dir)?;
    // 暫存檔 + rename：直接覆寫的話，正在跑的那支 shim 會讀到寫到一半的內容，而且中途死掉會留下
    // 一個不能執行的檔案（`shim_refresh::write_atomic`）。內容一樣就不動。
    crate::shim_refresh::write_atomic(&dir.join("cargo"), SHIM_SH)?;
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
            // `AM_REAL_CARGO` / `AM_BUILD_SLOT_HELD` 也要清掉：開發機的 shell 裡常設著（繞過 shim 跑
            // 測試時就會設），留著會讓 shim 改用真的 cargo，六條測試一起假紅。
            for key in ["AM_BOT_ID", "AM_HOOK_TOKEN", "AM_PORT", "AM_AGENT_NAME", "AM_REAL_CARGO", "AM_BUILD_SLOT_HELD", "AM_SHIM_DEPTH", "HOME"] {
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

    /// PATH 上還掛著**別顆 bot 的同一支 shim**（祖先 pane 繼承下來的，2026-09-18 實測有 6 個）：
    /// 真 cargo 要往後找，不能把另一份 shim 當成真 cargo——那會 shim → shim 一層層各拿一個名額，
    /// `max_concurrent` 被自己的外層佔滿，最內層永遠等不到（w168:p91 卡死 17 分鐘）。
    #[test]
    fn another_bots_shim_on_path_is_not_mistaken_for_the_real_cargo() {
        let s = Sandbox::new();
        // 另一顆 bot 的 bin 目錄，插在自己的 bin 與真 cargo 之間。
        let other = s.dir.join("other-bot");
        super::install_local(&other).unwrap();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":30}}' ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let mut cmd = std::process::Command::new(s.dir.join("bin/cargo"));
        cmd.env(
            "PATH",
            format!(
                "{}:{}:{}:/usr/bin:/bin",
                s.dir.join("bin").display(),
                other.join("bin").display(),
                s.dir.join("real").display()
            ),
        );
        for key in ["AM_BOT_ID", "AM_HOOK_TOKEN", "AM_PORT", "AM_AGENT_NAME", "AM_REAL_CARGO", "AM_BUILD_SLOT_HELD", "AM_SHIM_DEPTH", "HOME"] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", s.dir.join("fake-home"));
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        let out = cmd.args(["check", "-p", "agents-managerd"]).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "{err}");
        // 真 cargo 真的跑到了（不是卡在等名額），而且整趟只拿一個名額。
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "{err}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert_eq!(calls.matches("acquire").count(), 1, "一層一個名額就是死結：{calls}");
    }

    /// 2026-09-18 兩次死鎖的可重現版：PATH 上兩層 shim（其中一層是**舊版**、沒有
    /// `AM_SHIM_MARKER`，換版期間就是這樣混著），排程器只有兩個名額而且**不會**再多給。
    ///
    /// 舊的行為：每一層各拿一個名額，第三層永遠等——`cargo` 一次都沒跑到。現在只有最外層排隊，
    /// 真 cargo 一定跑得到，而且整趟只吃一個名額。
    #[test]
    fn two_layers_of_shims_with_only_two_slots_do_not_deadlock() {
        let s = Sandbox::new();
        // 第二層：別顆 bot 的 bin，而且是**舊版** shim（沒有 marker），只認得出檔頭的話會漏掉它。
        let other = s.dir.join("bots").join("OTHER").join("bin");
        std::fs::create_dir_all(&other).unwrap();
        let old_shim = super::SHIM_SH.replace("AM_SHIM_MARKER", "(舊版沒有這一行)");
        std::fs::write(other.join("cargo"), &old_shim).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(other.join("cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // 名額上限 2，而且拿滿就不再給——真的死鎖時這個測試會停在這裡（fake curl 不會 sleep，
        // shim 的 retry_after_secs=0，所以是一個忙等的迴圈，不是 10 分鐘的假等待）。
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*)
    n=$(grep -c acquire '{log}')
    if [ "$n" -le 2 ]; then printf '{{"granted":true,"token":"tok-$n","cargo_jobs":2,"lease_ttl_secs":30}}';
    else printf '{{"granted":false,"active":2,"retry_after_secs":0}}'; fi
    ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let mut cmd = std::process::Command::new(s.dir.join("bin/cargo"));
        cmd.env(
            "PATH",
            format!(
                "{}:{}:{}:/usr/bin:/bin",
                s.dir.join("bin").display(),
                other.display(),
                s.dir.join("real").display()
            ),
        );
        for key in ["AM_BOT_ID", "AM_HOOK_TOKEN", "AM_PORT", "AM_AGENT_NAME", "AM_REAL_CARGO", "AM_BUILD_SLOT_HELD", "AM_SHIM_DEPTH", "HOME"] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", s.dir.join("fake-home"));
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        let out = cmd.args(["build", "--release"]).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "{err}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("--release"), "真 cargo 沒跑到：{err}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert_eq!(calls.matches("acquire").count(), 1, "一層一個名額就是死結：{calls}");
    }

    /// build script／xtask 在拿著名額的 cargo 裡再叫一次 cargo：直接跑，不再排一次
    /// （內層等的名額要等外層結束才空，而外層在等內層）。
    #[test]
    fn a_nested_cargo_inside_a_held_slot_does_not_queue_again() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!("echo \"$@\" >> '{log}'\nprintf '{{}}'\n", log = call_log.display()));
        let cargo_log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_BUILD_SLOT_HELD", "1"),
                ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap()),
            ],
            &["build"],
        );
        assert_eq!(rc, 0, "{err}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("build"), "{err}");
        assert!(!s.dir.join("curl-calls.log").exists() || !std::fs::read_to_string(&call_log).unwrap().contains("acquire"), "不該再排一次");
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
