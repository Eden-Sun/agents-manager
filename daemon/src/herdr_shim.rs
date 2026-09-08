//! The `herdr` PATH shim (SPEC §6.5b).
//!
//! `lifecycle::child_agent_rules` asks an agent to name its children `<parent>-<suffix>` and the
//! reconcile picks them up from that. Asking is not a mechanism: an agent forgets, renames,
//! or was started as codex / grok and never read the rule, and the child becomes a sub-task
//! nobody can see. Descent (`reconcile`'s tab match) recovers those; this shim stops them
//! from happening — it sits at the front of a managed pane's PATH and rewrites the command
//! the agent actually typed.
//!
//! It also does what descent cannot: a pane herdr creates is spawned by the herdr *server*,
//! not by the calling shell, so a child pane inherits nothing. The shim passes the parent's
//! account (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`) and hook environment down with `--env`.

use std::path::{Path, PathBuf};

/// The shim itself. Kept verbatim so `sh` can be handed exactly what a pane will run.
pub const SHIM_SH: &str = r##"#!/bin/sh
# agents-manager herdr shim (SPEC §6.5b). Installed at the front of a managed pane's PATH.
#
# Naming a child agent `<parent>-<suffix>`, and handing a child pane the account and hook
# environment its parent runs under, used to be a *request* written into the agent's persona.
# Here they are a mechanism: whatever the agent types, the child comes out named and carrying
# the env that makes it trackable.
#
# POSIX sh only — a pane's shell may be zsh, bash, dash or ash — and no `set -e`: a shim that
# aborts is worse than one that forwards.

# The real herdr: `$AM_REAL_HERDR` if the daemon resolved one, else the first `herdr` on PATH
# that is not this directory (otherwise we would exec ourselves forever).
am_real_herdr() {
    if [ -n "${AM_REAL_HERDR:-}" ] && [ -x "$AM_REAL_HERDR" ]; then
        printf '%s\n' "$AM_REAL_HERDR"
        return 0
    fi
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    printf '%s\n' "$PATH" | tr ':' '\n' | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            if [ -x "$_abs/herdr" ]; then
                printf '%s\n' "$_abs/herdr"
                break
            fi
        done
    }
}

# `<name>` → `<AM_AGENT_NAME>-<name>`, unless it already carries the prefix. herdr agent names
# are `[a-z][a-z0-9_-]{0,31}`, so the result is cut to 32.
am_child_name() {
    _n=$1
    if [ -z "${AM_AGENT_NAME:-}" ]; then
        printf '%s' "$_n"
        return 0
    fi
    case "$_n" in
        "$AM_AGENT_NAME"-*)
            printf '%s' "$_n"
            return 0
            ;;
    esac
    _full=$(printf '%s-%s' "$AM_AGENT_NAME" "$_n" | cut -c1-32)
    printf 'agents-manager: 子 agent 已改名為 `%s`，才會掛在 `%s` 底下被追蹤\n' "$_full" "$AM_AGENT_NAME" >&2
    printf '%s' "$_full"
}

# `herdr agent start [flags] <name> …` — rewrite the first bare word after `start`, which is
# the agent name. Flags may come first, so skip options and the values of the ones that take
# one, and stop at `--` (everything after it is the agent's own argv).
am_agent_start() {
    shift 2
    _n=$#
    _i=0
    _named=0
    _stop=0
    _prev=""
    _kind=""
    _has_model=0
    _has_effort=0
    while [ "$_i" -lt "$_n" ]; do
        _a=$1
        shift
        _i=$((_i + 1))
        _orig=$_a
        if [ "$_prev" = "--kind" ]; then _kind=$_a; fi
        if [ "$_stop" = 1 ]; then
            case "$_a" in
                --model | --model=*) _has_model=1 ;;
                --effort | --effort=*) _has_effort=1 ;;
                -c) : ;;
                model_reasoning_effort=*) _has_effort=1 ;;
            esac
        elif [ "$_named" = 0 ]; then
            case "$_prev" in
                --kind | --pane | --timeout) : ;;
                *)
                    case "$_a" in
                        --) _stop=1 ;;
                        -*) : ;;
                        *)
                            _a=$(am_child_name "$_a")
                            _named=1
                            ;;
                    esac
                    ;;
            esac
        else
            case "$_a" in
                --) _stop=1 ;;
            esac
        fi
        _prev=$_orig
        set -- "$@" "$_a"
    done
    # 沒指定模型的子 agent 會跑 CLI 的預設（claude 現在是 fable），跟母 bot 明明選的 opus 對不上，
    # 側欄就多出一顆「claude-fable-5-1」看不懂的。母 bot 的模型／強度在 AM_MODEL / AM_EFFORT，
    # 同 kind 就補上；自己有寫 --model 的一律尊重。
    if [ -n "${AM_MODEL:-}" ] && [ "$_has_model" = 0 ] && { [ -z "$_kind" ] || [ "$_kind" = "${AM_KIND:-}" ]; }; then
        [ "$_stop" = 1 ] || set -- "$@" --
        set -- "$@" --model "$AM_MODEL"
        if [ -n "${AM_EFFORT:-}" ] && [ "$_has_effort" = 0 ] && [ "${AM_KIND:-}" = "claude" ]; then
            set -- "$@" --effort "$AM_EFFORT"
        fi
        printf 'agents-manager: 子 agent 沒指定模型，沿用母 bot 的 `%s`\n' "$AM_MODEL" >&2
    fi
    exec "$AM_HERDR" agent start "$@"
}

# herdr spawns a pane from the *server*, not from this shell, so nothing is inherited: a child
# pane would come up on the user's default account, with no hook token and no way to name its
# own children. Pass the parent's environment down explicitly, without overriding a value the
# caller set by hand.
am_forward_with_env() {
    for _k in CLAUDE_CONFIG_DIR CODEX_HOME AM_BOT_ID AM_HOOK_TOKEN AM_PORT AM_RUN_ID AM_AGENT_NAME AM_KIND AM_MODEL AM_EFFORT AM_REAL_HERDR PATH; do
        eval "_v=\${$_k:-}"
        [ -n "$_v" ] || continue
        case " $* " in
            *" $_k="*) continue ;;
        esac
        set -- "$@" --env "$_k=$_v"
    done
    exec "$AM_HERDR" "$@"
}

AM_HERDR=$(am_real_herdr | head -n 1)
if [ -z "$AM_HERDR" ]; then
    printf 'agents-manager: 找不到真正的 herdr（把它的路徑放進 AM_REAL_HERDR）\n' >&2
    exit 127
fi

case "${1:-} ${2:-}" in
    "agent start") am_agent_start "$@" ;;
    "pane split" | "pane new" | "tab create") am_forward_with_env "$@" ;;
    *) exec "$AM_HERDR" "$@" ;;
esac
"##;

/// Write the shim into `<bot dir>/bin/herdr` and return that directory, which is what goes on
/// the pane's PATH. Idempotent: same bytes, same mode, rewritten every start so an upgraded
/// daemon never leaves an old shim behind.
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = bot_dir.join("bin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("herdr");
    std::fs::write(&path, SHIM_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// The remote half, over the same ssh path `hook.sh` takes (SPEC §11.4): `<remote bot
/// dir>/bin/herdr`. Returns the directory to prepend to that host's pane PATH.
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let dir = format!("{remote_bot_dir}/bin");
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/herdr\" <<'AM_SHIM_EOF'\n{shim}AM_SHIM_EOF\nchmod +x \"$D/herdr\"\nprintf 'AM_SHIM_INSTALLED\\n'\n",
        dir = crate::hosts::sh_quote(&dir),
        shim = SHIM_SH,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_SHIM_INSTALLED") {
        anyhow::bail!("remote herdr shim install did not confirm:\n{}", out.trim());
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    //! The shim is a shell script, so the test runs it with `sh` against a fake `herdr` that
    //! prints its argv one per line. Anything less would only be testing a Rust string.
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
            let dir = std::env::temp_dir().join(format!("am-shim-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            super::install_local(&dir).unwrap();
            let fake = dir.join("real");
            std::fs::create_dir_all(&fake).unwrap();
            let mut f = std::fs::File::create(fake.join("herdr")).unwrap();
            f.write_all(b"#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n").unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(fake.join("herdr"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Sandbox { dir }
        }

        /// Run the shim with `AM_AGENT_NAME` set, returning `(stdout lines, stderr)`.
        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String) {
            let mut cmd = Command::new(self.dir.join("bin/herdr"));
            // The real herdr is behind the shim's own directory, exactly as on a pane.
            let path = format!(
                "{}:{}:/usr/bin:/bin",
                self.dir.join("bin").display(),
                self.dir.join("real").display()
            );
            cmd.env("PATH", path).args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect();
            (stdout, String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    /// The rule the persona could only ask for: a child agent comes out prefixed whatever the
    /// agent typed, and the agent is told so on stderr.
    #[test]
    fn agent_start_prefixes_the_child_name() {
        let s = Sandbox::new();
        let (out, err) =
            s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", "review", "--kind", "claude"]);
        assert_eq!(out, ["agent", "start", "proj-abc123-review", "--kind", "claude"]);
        assert!(err.contains("proj-abc123-review"), "the rename is announced: {err}");
    }

    /// A name that already carries the prefix is left alone — and so is the argv order, flags
    /// before the name included, plus everything after `--` (the agent's own argv).
    #[test]
    fn a_prefixed_name_and_the_argv_order_are_left_alone() {
        let s = Sandbox::new();
        let (out, err) =
            s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", "proj-abc123-ui", "--kind", "codex"]);
        assert_eq!(out, ["agent", "start", "proj-abc123-ui", "--kind", "codex"]);
        assert_eq!(err, "");

        let (out, _) = s.run(
            &[("AM_AGENT_NAME", "p-1")],
            &["agent", "start", "--kind", "claude", "--pane", "w1:p3", "ui", "--", "--model", "opus"],
        );
        assert_eq!(out, ["agent", "start", "--kind", "claude", "--pane", "w1:p3", "p-1-ui", "--", "--model", "opus"]);
    }

    /// 2026-09-08：子 agent 沒帶 `--model` 就跑 CLI 預設，側欄多一顆「claude-fable-5-1」。母 bot 的
    /// 模型從 `AM_MODEL` / `AM_EFFORT` 補上；同 kind 才補，自己有寫的不動。
    #[test]
    fn a_child_without_a_model_inherits_the_parents() {
        let s = Sandbox::new();
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_KIND", "claude"), ("AM_MODEL", "opus"), ("AM_EFFORT", "medium")];
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "claude"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--", "--model", "opus", "--effort", "medium"]);
        assert!(err.contains("沿用母 bot"), "{err}");

        // 自己寫了 --model：不動，也不補 effort 以外的東西。
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--", "--model", "sonnet"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--", "--model", "sonnet"]);

        // 不同 kind：母 bot 的模型名對它沒意義。
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "codex"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "codex"]);

        // 已經有 `--` 但沒有 --model：接在後面。
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--", "--verbose"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--", "--verbose", "--model", "opus", "--effort", "medium"]);
    }

    /// `--pane w1:p3` is a *value*, not the agent name; renaming it would target a pane that
    /// does not exist.
    #[test]
    fn an_option_value_is_never_mistaken_for_the_name() {
        let s = Sandbox::new();
        let (out, _) = s.run(&[("AM_AGENT_NAME", "p-1")], &["agent", "start", "--pane", "w1:p3", "kid"]);
        assert_eq!(out, ["agent", "start", "--pane", "w1:p3", "p-1-kid"]);
    }

    /// herdr agent names are `[a-z][a-z0-9_-]{0,31}`; a long suffix is cut, not rejected.
    #[test]
    fn a_long_name_is_cut_to_herdrs_32_characters() {
        let s = Sandbox::new();
        let (out, _) = s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", &"x".repeat(40)]);
        assert_eq!(out[2].len(), 32);
        assert!(out[2].starts_with("proj-abc123-x"));
    }

    /// A pane herdr creates is spawned by the server, so it inherits nothing: without this the
    /// child would come up on the user's default claude account and with no hook token.
    #[test]
    fn a_new_pane_carries_the_parents_account_and_hook_env() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[
                ("AM_AGENT_NAME", "p-1"),
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_PORT", "7788"),
                ("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2"),
            ],
            &["pane", "split", "--pane", "w1:p1", "--direction", "right"],
        );
        assert_eq!(&out[..6], ["pane", "split", "--pane", "w1:p1", "--direction", "right"]);
        for want in ["CLAUDE_CONFIG_DIR=/home/u/.claude-cc2", "AM_BOT_ID=b1", "AM_HOOK_TOKEN=tok", "AM_PORT=7788"] {
            assert!(out.contains(&want.to_string()), "{want} was not passed down: {out:?}");
        }
    }

    /// A value the agent set by hand wins: the shim fills gaps, it does not overrule.
    #[test]
    fn an_env_the_caller_set_is_not_overridden() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("CLAUDE_CONFIG_DIR", "/home/u/.claude")],
            &["tab", "create", "--workspace", "w1", "--env", "CLAUDE_CONFIG_DIR=/other"],
        );
        assert_eq!(out.iter().filter(|a| a.starts_with("CLAUDE_CONFIG_DIR=")).count(), 1);
        assert!(out.contains(&"CLAUDE_CONFIG_DIR=/other".to_string()));
    }

    /// Everything else is the real herdr, untouched — and the shim must never find *itself*.
    #[test]
    fn every_other_subcommand_is_forwarded_verbatim() {
        let s = Sandbox::new();
        let (out, err) = s.run(&[("AM_AGENT_NAME", "p-1")], &["agent", "list"]);
        assert_eq!(out, ["agent", "list"]);
        assert_eq!(err, "");
    }
}
