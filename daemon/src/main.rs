//! agents-managerd — a local multi-agent manager on top of herdr.
//!
//! Subcommands:
//!   serve                       run the daemon (REST + WS + hook receiver)
//!   hook claude|codex ...       the tiny process agent CLIs invoke; always exits 0
//!
//! 薄 bin：只做 CLI 解析與派發，其餘都在 `agents_managerd` 函式庫（lib.rs）。

use agents_managerd::{herdr_update, hook_cmd, release_triage, remote_cargo, serve, statusline_cmd, Cli, Cmd};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook { provider, bot, token, port, data_dir, event, payload } => {
            let payload_arg = if provider == "codex" { payload.last().cloned() } else { None };
            hook_cmd::run(hook_cmd::HookArgs { provider, bot, token, port, data_dir, event, payload_arg });
            std::process::exit(0);
        }
        Cmd::Statusline { bot, token, port, data_dir: _ } => {
            statusline_cmd::run(statusline_cmd::StatuslineArgs { bot, token, port });
            std::process::exit(0);
        }
        Cmd::RemoteCargo { config, data_dir, cwd, cargo_args } => {
            std::process::exit(remote_cargo::run_cli(&config, data_dir.as_deref(), &cwd, &cargo_args));
        }
        Cmd::HerdrUpdateCheck { installed, latest, changelog_file, last_notified } => {
            let md = std::fs::read_to_string(&changelog_file).unwrap_or_else(|e| {
                eprintln!("讀不了 {}: {e}", changelog_file.display());
                std::process::exit(2);
            });
            let Some(report) = herdr_update::build_report(&installed, &latest, &md) else {
                eprintln!("看不懂版本號：installed=`{installed}` latest=`{latest}`");
                std::process::exit(2);
            };
            let should_notify = herdr_update::should_notify(&report, last_notified.as_deref());
            let brief = should_notify.then(|| herdr_update::render_agm_brief(&report));
            println!(
                "{}",
                serde_json::json!({
                    "installed_version": report.installed_version,
                    "latest_version": report.latest_version,
                    "has_update": report.has_update,
                    "should_notify": should_notify,
                    "brief": brief,
                })
            );
            std::process::exit(0);
        }
        Cmd::ReleaseTriageCheck { kind, since, json: _, installed, db, feed_file } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            match rt.block_on(release_triage::run_check(release_triage::CheckArgs { kind, since, installed, db, feed_file })) {
                Ok(report) => {
                    println!("{}", serde_json::to_string(&report).expect("serialize CheckReport"));
                    std::process::exit(0);
                }
                Err(e) => {
                    // 結構化錯誤放 stdout（kick 讀 stdout），exit 1 照舊：不是「沒有新版」。
                    println!("{}", serde_json::json!({"error": format!("{e:#}")}));
                    eprintln!("release-triage-check: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Serve { config, dev_watch_all_panes } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(serve::serve(config, dev_watch_all_panes)) {
                eprintln!("fatal: {e:?}");
                std::process::exit(1);
            }
        }
    }
}

