//! Shared host-side operations. Exec is the only transport boundary: local sh or this host's SSH.
use super::*;
use crate::hosts::{sh_quote, HostConn};

pub(super) trait Exec: Send + Sync {
    fn run<'a>(
        &'a self,
        script: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>>;
    /// 跟 `run` 一樣，但帶這台主機設定的 `remote_path`（跟版本探測找 herdr 用同一條 PATH）。
    fn run_with_path<'a>(
        &'a self,
        script: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.run(script, timeout)
    }
}

struct HostExec(Arc<HostConn>);
impl Exec for HostExec {
    fn run<'a>(
        &'a self,
        script: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if self.0.is_local() {
                local_exec(script, timeout).await
            } else {
                self.0
                    .ssh_exec_timeout(script, timeout)
                    .await
                    .map_err(|e| format!("SSH {}：{e:#}", self.0.name))
            }
        })
    }
    fn run_with_path<'a>(
        &'a self,
        script: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if self.0.is_local() {
                local_exec(script, timeout).await
            } else {
                self.0
                    .ssh_exec_path_timeout(script, timeout)
                    .await
                    .map_err(|e| self.ssh_err(e))
            }
        })
    }
}

impl HostExec {
    fn ssh_err(&self, e: anyhow::Error) -> String {
        format!("SSH {}：{e:#}", self.0.name)
    }
}

async fn local_exec(script: &str, timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.args(["-c", script]);
    command_output(cmd, timeout).await
}

pub(super) struct Real {
    exec: Arc<dyn Exec>,
    remote_root: Option<String>,
}
impl Real {
    pub(super) fn new(conn: Arc<HostConn>) -> Self {
        let remote_root =
            (!conn.is_local()).then(|| crate::hosts::remote_root_for(conn.instance()));
        Self {
            exec: Arc::new(HostExec(conn)),
            remote_root,
        }
    }
    #[cfg(test)]
    pub(super) fn local() -> Self {
        struct Local;
        impl Exec for Local {
            fn run<'a>(
                &'a self,
                script: &'a str,
                timeout: Duration,
            ) -> BoxFuture<'a, Result<String, String>> {
                Box::pin(local_exec(script, timeout))
            }
        }
        Self {
            exec: Arc::new(Local),
            remote_root: None,
        }
    }
    async fn staging_dir(&self, app: &Arc<App>, version: &str) -> Result<PathBuf, String> {
        match &self.remote_root {
            Some(root) => {
                let home = self
                    .exec
                    .run("printf '%s' \"$HOME\"", Duration::from_secs(15))
                    .await?;
                if !home.starts_with('/') || home.trim().is_empty() {
                    return Err("遠端 HOME 不是絕對路徑".into());
                }
                Ok(PathBuf::from(home)
                    .join(root)
                    .join(STAGING_DIR)
                    .join(version))
            }
            None => Ok(app.data_dir.join(STAGING_DIR).join(version)),
        }
    }
    async fn asset(&self) -> Result<String, String> {
        let platform = self
            .exec
            .run("uname -s; uname -m", Duration::from_secs(15))
            .await?;
        let mut lines = platform.lines();
        let os = match lines.next() {
            Some("Darwin") => "macos",
            Some("Linux") => "linux",
            _ => return Err(format!("沒有對應的 release asset：{platform}")),
        };
        let arch = match lines.next() {
            Some("arm64" | "aarch64") => "aarch64",
            Some("x86_64") => "x86_64",
            _ => return Err(format!("沒有對應的 release asset：{platform}")),
        };
        Ok(format!("herdr-{os}-{arch}"))
    }
}

/// Resolve both files on the target host, rather than in the daemon's filesystem.
const RESOLVE: &str = "perl -MCwd=abs_path -e 'my $a=abs_path($ARGV[0]); my $b=abs_path($ARGV[1]); die \"cannot resolve service program\\n\" unless defined($a) && defined($b); die \"service program differs from install binary\\n\" unless $a eq $b'";

/// Sets `$program` to the binary the session's launchd job / systemd unit executes. Needs `uid`, `XDG_RUNTIME_DIR` set.
fn program_lookup(label: &str, unit: &str) -> String {
    format!(
        r#"
case "$(uname -s)" in
Darwin)
  job=$(launchctl print "gui/$uid/"{label})
  program=$(printf '%s\n' "$job" | sed -n 's/^[[:space:]]*program = //p' | sed 's/[[:space:]]*$//' | head -1)
  ;;
Linux)
  test "$(systemctl --user is-active {unit})" = active
  job=$(systemctl --user show {unit} --property=ExecStart --value)
  program=$(printf '%s\n' "$job" | sed -n 's/.*path=\([^;]*\);.*/\1/p' | sed 's/[[:space:]]*$//' | head -1)
  ;;
*) echo 'unsupported service platform' >&2; exit 1 ;;
esac
test -n "$program"
"#
    )
}

fn service_names(session: &str) -> Result<(String, String), String> {
    if !session_ok(session) {
        return Err(format!(
            "session 名 `{session}` 不能當 systemd/launchd 名稱"
        ));
    }
    Ok((
        sh_quote(&format!("herdr@{session}.service")),
        sh_quote(&format!("dev.agents-manager.herdr-{session}")),
    ))
}

const SERVICE_PRELUDE: &str = "set -eu\nuid=$(id -u)\nexport XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$uid}\n";

/// Prints the (regular-file) binary the session's server really runs; a package-manager symlink is refused.
pub(super) fn program_script(session: &str) -> Result<String, String> {
    let (unit, label) = service_names(session)?;
    Ok(format!(
        r#"{SERVICE_PRELUDE}{lookup}
case "$program" in /*) ;; *) echo "service program is not an absolute path: $program" >&2; exit 1 ;; esac
if test -L "$program" || ! test -f "$program"; then echo "server 跑的 $program is not a regular file (package manager install)" >&2; exit 1; fi
printf '%s' "$program"
"#,
        lookup = program_lookup(&label, &unit)
    ))
}

/// Prints the CLI that login shells / shims resolve (same lookup as the version probe) only when it is a different
/// file than `server`; prints nothing when it is the same file or there is none.
pub(super) fn cli_script(server: &Path) -> String {
    format!(
        r#"set -eu
hp=$( "${{SHELL:-/bin/sh}}" -lic "command -v herdr" 2>/dev/null | tail -1 )
case "$hp" in /*) ;; *) hp="" ;; esac
[ -n "$hp" ] || hp=$(command -v herdr 2>/dev/null || true)
case "$hp" in /*) ;; *) hp="" ;; esac
[ -n "$hp" ] || exit 0
a=$(perl -MCwd=abs_path -e 'print abs_path($ARGV[0])' "$hp")
b=$(perl -MCwd=abs_path -e 'print abs_path($ARGV[0])' {server})
if test -n "$a" && test "$a" = "$b"; then exit 0; fi
if test -L "$hp" || ! test -f "$hp"; then echo "CLI／shim 解析到的 $hp is not a regular file (package manager install)" >&2; exit 1; fi
printf '%s' "$hp"
"#,
        server = sh_quote(&server.to_string_lossy())
    )
}

pub(super) fn service_script(session: &str, install: Option<&Path>) -> Result<String, String> {
    let (unit, label) = service_names(session)?;
    let action = match install {
        Some(install) => format!(
            "{lookup}{RESOLVE} \"$program\" {install}\n",
            lookup = program_lookup(&label, &unit),
            install = sh_quote(&install.to_string_lossy())
        ),
        // macOS：換了 binary 之後 `kickstart -k` 會因為 job 還綁著舊 binary 的簽章 spawn failed（OS_REASON_CODESIGNING），
        // 要 bootout 再 bootstrap。bootout 是非同步的，bootstrap 失敗就等一下重試；最後仍失敗＝這一步失敗，呼叫端會換回舊版。
        None => format!(
            r#"
case "$(uname -s)" in
Darwin)
  plist=$(launchctl print "gui/$uid/"{label} | sed -n 's/^[[:space:]]*path = //p' | sed 's/[[:space:]]*$//' | head -1)
  case "$plist" in *.plist) ;; *) plist="$HOME/Library/LaunchAgents/"{label}".plist" ;; esac
  test -f "$plist"
  launchctl bootout "gui/$uid/"{label} || true
  n=0
  until launchctl bootstrap "gui/$uid" "$plist"; do
    n=$((n+1))
    test "$n" -lt 10
    sleep 1
  done
  ;;
Linux) systemctl --user restart {unit} ;;
*) echo 'unsupported service platform' >&2; exit 1 ;;
esac
"#
        ),
    };
    Ok(format!("{SERVICE_PRELUDE}{action}"))
}

pub(super) fn swap_script(staging: &Path, install: &Path, backup: &Path) -> String {
    let fresh = install.with_file_name(format!(".herdr.new-{}", crate::db::ulid()));
    format!(
        r#"set -eu
install={install}
backup={backup}
fresh={fresh}
trap 'rm -f "$fresh"' EXIT
# Refuse a package-managed symlink even if it changed after preflight.
test -f "$install" && test ! -L "$install"
rm -f "$backup"
ln "$install" "$backup" 2>/dev/null || cp -p "$install" "$backup"
cp {staging} "$fresh"
chmod 755 "$fresh"
mv -f "$fresh" "$install"
"#,
        install = sh_quote(&install.to_string_lossy()),
        backup = sh_quote(&backup.to_string_lossy()),
        fresh = sh_quote(&fresh.to_string_lossy()),
        staging = sh_quote(&staging.to_string_lossy())
    )
}

impl Ops for Real {
    fn install_path<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let path = self
                .exec
                .run(&program_script(session)?, Duration::from_secs(15))
                .await?;
            let p = PathBuf::from(path.trim());
            if !p.is_absolute() {
                return Err("binary 路徑不是絕對路徑".into());
            }
            Ok(p)
        })
    }
    fn cli_path<'a>(&'a self, server: &'a Path) -> BoxFuture<'a, Result<Option<PathBuf>, String>> {
        Box::pin(async move {
            let out = self
                .exec
                .run_with_path(&cli_script(server), Duration::from_secs(15))
                .await?;
            let p = out.trim();
            if p.is_empty() {
                return Ok(None);
            }
            let p = PathBuf::from(p);
            if !p.is_absolute() {
                return Err("CLI 路徑不是絕對路徑".into());
            }
            Ok(Some(p))
        })
    }
    fn staging<'a>(
        &'a self,
        app: &'a Arc<App>,
        version: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let dir = self.staging_dir(app, version).await?;
            self.exec
                .run(
                    &format!("set -eu\nmkdir -p {}", sh_quote(&dir.to_string_lossy())),
                    Duration::from_secs(15),
                )
                .await?;
            Ok(dir.join("herdr"))
        })
    }
    fn cleanup<'a>(
        &'a self,
        app: &'a Arc<App>,
        version: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let dir = self.staging_dir(app, version).await?;
            self.exec
                .run(
                    &format!("rm -rf {}", sh_quote(&dir.to_string_lossy())),
                    Duration::from_secs(15),
                )
                .await
                .map(|_| ())
        })
    }
    fn download<'a>(
        &'a self,
        version: &'a str,
        dest: &'a Path,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let asset = self.asset().await?;
            let url = format!("{RELEASE_BASE}/v{version}/{asset}");
            let part = dest.with_extension("part");
            let args = curl_args(&url, &part)
                .iter()
                .map(|s| sh_quote(s))
                .collect::<Vec<_>>()
                .join(" ");
            self.exec
                .run(
                    &format!(
                        "set -eu\ncurl {args}\nchmod 755 {}\nmv -f {} {}",
                        sh_quote(&part.to_string_lossy()),
                        sh_quote(&part.to_string_lossy()),
                        sh_quote(&dest.to_string_lossy())
                    ),
                    Duration::from_secs(320),
                )
                .await
                .map(|_| ())
        })
    }
    fn verify_download<'a>(
        &'a self,
        version: &'a str,
        path: &'a Path,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let asset = self.asset().await?;
            let url = sh_quote(&format!(
                "https://api.github.com/repos/herdrdev/herdr/releases/tags/v{version}"
            ));
            let body = self.exec.run(&format!("curl -fsSL --proto '=https' --proto-redir '=https' --max-time 30 -H 'Accept: application/vnd.github+json' {url}"), Duration::from_secs(40)).await?;
            let want = published_digest(&body, &asset)?;
            let digest = self.exec.run(&format!("set -eu\nif command -v sha256sum >/dev/null 2>&1; then sha256sum {}; else shasum -a 256 {}; fi", sh_quote(&path.to_string_lossy()), sh_quote(&path.to_string_lossy())), Duration::from_secs(30)).await?;
            let got = digest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if got == want {
                Ok(())
            } else {
                Err(format!("{asset} sha256 是 {got}，公布的是 {want}"))
            }
        })
    }
    fn preflight_restart<'a>(
        &'a self,
        session: &'a str,
        install: &'a Path,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.exec
                .run(
                    &service_script(session, Some(install))?,
                    Duration::from_secs(15),
                )
                .await
                .map(|_| ())
        })
    }
    fn binary_version<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let script = format!(
                r#"set -u
attempt=0
while test "$attempt" -lt 20; do
  if out=$({path} --version 2>&1); then printf '%s' "$out"; exit 0; fi
  case "$out" in *'Text file busy'*|*'text file busy'*) attempt=$((attempt+1)); sleep 0.1 ;; *) printf '%s' "$out" >&2; exit 1 ;; esac
done
printf '%s' "$out" >&2; exit 1
"#,
                path = sh_quote(&path.to_string_lossy())
            );
            let out = self.exec.run(&script, Duration::from_secs(15)).await?;
            cli_version_string(&out).ok_or_else(|| format!("看不懂 --version：{out}"))
        })
    }
    fn swap<'a>(
        &'a self,
        staging: &'a Path,
        install: &'a Path,
        from: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let name = install
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("binary 沒有檔名")?;
            let backup = install.with_file_name(format!("{name}.bak-{from}"));
            self.exec
                .run(
                    &swap_script(staging, install, &backup),
                    Duration::from_secs(60),
                )
                .await?;
            Ok(backup)
        })
    }
    fn swap_outcome_uncertain(&self) -> bool {
        self.remote_root.is_some()
    }
    fn rollback<'a>(
        &'a self,
        backup: &'a Path,
        install: &'a Path,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let fresh = install.with_file_name(format!(".herdr.rollback-{}", crate::db::ulid()));
            self.exec.run(&format!("set -eu\nfresh={}\ntrap 'rm -f \"$fresh\"' EXIT\ncp {} \"$fresh\"\nchmod 755 \"$fresh\"\nmv -f \"$fresh\" {}", sh_quote(&fresh.to_string_lossy()), sh_quote(&backup.to_string_lossy()), sh_quote(&install.to_string_lossy())), Duration::from_secs(60)).await.map(|_| ())
        })
    }
    fn restart_server<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.exec
                .run(&service_script(session, None)?, Duration::from_secs(60))
                .await
                .map(|_| ())
        })
    }
    fn resume<'a>(
        &'a self,
        app: &'a Arc<App>,
        bot_id: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(crate::app_ports_p12::restart_bot_resume_native(app, bot_id))
    }
    fn notify_parent<'a>(
        &'a self,
        app: &'a Arc<App>,
        parent: &'a str,
        text: &'a str,
        request: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(crate::app_ports_p12::notify_parent_relayed(
            app, parent, text, request,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_operations_all_use_the_selected_hosts_ssh_and_remote_paths() {
        let env = crate::testing::env().await;
        let name = format!("upgrade-{}", crate::db::ulid());
        let cfg = crate::config::HostCfg { daemon_url: None,
            name: name.clone(),
            ssh: "never-connect.invalid".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "remote-owner".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = calls.clone();
        crate::hosts::set_ssh_fake(&name, move |script| {
            captured.lock().unwrap().push(script.into());
            Ok(if script.contains("printf '%s' \"$program\"") {
                "/opt/remote/bin/herdr".into()
            } else if script.contains("command -v herdr") {
                "/remote/home/.local/bin/herdr".into()
            } else if script == "printf '%s' \"$HOME\"" {
                "/remote/home".into()
            } else if script == "uname -s; uname -m" {
                "Darwin\narm64\n".into()
            } else if script.contains("api.github.com") {
                format!(
                    r#"{{"assets":[{{"name":"herdr-macos-aarch64","digest":"sha256:{}"}}]}}"#,
                    "ab".repeat(32)
                )
            } else if script.contains("shasum") {
                format!("{}  remote-herdr\n", "ab".repeat(32))
            } else if script.contains("--version 2>&1") {
                "herdr 0.9.3\n".into()
            } else {
                String::new()
            })
        });
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        let real = Real::new(conn);
        let install = real.install_path("remote-owner").await.unwrap();
        assert_eq!(install, Path::new("/opt/remote/bin/herdr"));
        let cli = real.cli_path(&install).await.unwrap();
        assert_eq!(
            cli.as_deref(),
            Some(Path::new("/remote/home/.local/bin/herdr")),
            "CLI 另外一份也要看"
        );
        real.preflight_restart("remote-owner", &install)
            .await
            .unwrap();
        let staged = real.staging(&env.app, "0.9.3").await.unwrap();
        assert_eq!(
            staged,
            Path::new("/remote/home/.config/agents-manager/herdr-staging/0.9.3/herdr")
        );
        real.download("0.9.3", &staged).await.unwrap();
        real.verify_download("0.9.3", &staged).await.unwrap();
        assert_eq!(real.binary_version(&staged).await.unwrap(), "0.9.3");
        let backup = real.swap(&staged, &install, "0.9.1").await.unwrap();
        real.restart_server("remote-owner").await.unwrap();
        real.rollback(&backup, &install).await.unwrap();
        real.cleanup(&env.app, "0.9.3").await.unwrap();
        let scripts = calls.lock().unwrap().join("\n");
        assert!(
            scripts.contains("herdr-macos-aarch64"),
            "target platform, not daemon platform"
        );
        assert!(scripts.contains("--proto-redir") && scripts.contains("=https"));
        assert!(scripts.contains("dev.agents-manager.herdr-remote-owner"));
        assert!(scripts.contains("herdr@remote-owner.service"));
        assert!(
            scripts.contains("launchctl bootout") && scripts.contains("launchctl bootstrap"),
            "macOS 換 binary 後要 bootout＋bootstrap"
        );
        assert!(!scripts.contains("kickstart"), "kickstart 會因舊簽章 spawn failed");
        assert!(scripts.contains(".bak-0.9.1"));
        assert!(
            !scripts.contains(&env.app.data_dir.to_string_lossy().to_string()),
            "local data directory must never be sent to remote"
        );
        assert!(
            !staged.exists(),
            "all remote IO was replaced; nothing written locally"
        );
    }

    fn mock_bin(dir: &Path, name: &str, body: &str) {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        set_exec(&p).unwrap();
    }

    /// Execute production service scripts against fake tools only, on both platforms.
    #[tokio::test]
    async fn macos_local_service_scripts_check_job_and_resolved_binary_and_restart_only_selected_session(
    ) {
        let dir = crate::testing::scratch_dir("herdr-service-scripts").join("with spaces");
        std::fs::create_dir_all(&dir).unwrap();
        let install = dir.join("herdr");
        let other = dir.join("other-herdr");
        std::fs::write(&install, "old").unwrap();
        std::fs::write(&other, "other").unwrap();
        let marker = dir.join("calls");
        let prefix = format!(
            "PATH={}:$PATH\nexport PATH\n",
            sh_quote(&dir.to_string_lossy())
        );
        mock_bin(&dir, "id", "echo 123");
        let plist = dir.join("dev.agents-manager.herdr-remote-owner.plist");
        std::fs::write(&plist, "<plist/>").unwrap();
        for platform in ["Darwin", "Linux"] {
            mock_bin(&dir, "uname", &format!("echo {platform}"));
            for (program, active) in [(&install, true), (&other, true), (&install, false)] {
                let program = program.to_string_lossy();
                mock_bin(
                    &dir,
                    "launchctl",
                    &format!(
                        r#"
if test "$1" = print; then
  {active} || exit 1
  printf 'path = %s\n' {plist}
  printf 'program = %s\n' {program}
else
  printf '%s\n' "$*" >> {marker}
fi"#,
                        active = if active { "true" } else { "false" },
                        program = sh_quote(&program),
                        plist = sh_quote(&plist.to_string_lossy()),
                        marker = sh_quote(&marker.to_string_lossy())
                    ),
                );
                mock_bin(
                    &dir,
                    "systemctl",
                    &format!(
                        r#"
case "$2" in
is-active) {active} || exit 1; echo active ;;
show) printf '{{ path=%s ; argv[]=herdr --session remote-owner server ; }}\n' {program} ;;
restart) printf '%s\n' "$*" >> {marker} ;;
*) exit 1 ;;
esac"#,
                        active = if active { "true" } else { "false" },
                        program = sh_quote(&program),
                        marker = sh_quote(&marker.to_string_lossy())
                    ),
                );
                let script =
                    prefix.clone() + &service_script("remote-owner", Some(&install)).unwrap();
                let result = local_exec(&script, Duration::from_secs(5)).await;
                assert_eq!(
                    result.is_ok(),
                    active && program == install.to_string_lossy(),
                    "{platform}: {result:?}"
                );
                if result.is_ok() {
                    local_exec(
                        &(prefix.clone() + &service_script("remote-owner", None).unwrap()),
                        Duration::from_secs(5),
                    )
                    .await
                    .unwrap();
                }
            }
        }
        let calls = std::fs::read_to_string(marker).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "bootout gui/123/dev.agents-manager.herdr-remote-owner".to_string(),
                format!("bootstrap gui/123 {}", plist.display()),
                "--user restart herdr@remote-owner.service".to_string(),
            ]
        );
        assert!(service_script("bad/session", None).is_err());
    }

    /// A sandbox: fake `uname`/`id`/`systemctl` and a fake login shell that prints `$MOCK_HERDR` for `command -v herdr`.
    struct Sandbox {
        dir: PathBuf,
        shell_answer: Mutex<String>,
    }
    impl Exec for Sandbox {
        fn run<'a>(
            &'a self,
            script: &'a str,
            timeout: Duration,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                let mut cmd = tokio::process::Command::new("/bin/sh");
                let path = format!("{}:/usr/bin:/bin", self.dir.display());
                cmd.args(["-c", script])
                    .env("HOME", &self.dir)
                    .env("PATH", path)
                    .env("SHELL", self.dir.join("shell"))
                    .env("MOCK_HERDR", self.shell_answer.lock().unwrap().clone());
                command_output(cmd, timeout).await
            })
        }
    }

    fn sandbox(name: &str) -> (Real, PathBuf, Arc<Sandbox>) {
        let dir = crate::testing::scratch_dir(name);
        mock_bin(&dir, "uname", "echo Linux");
        mock_bin(&dir, "id", "echo 123");
        mock_bin(&dir, "shell", r#"test -z "$MOCK_HERDR" || printf '%s\n' "$MOCK_HERDR""#);
        let sb = Arc::new(Sandbox {
            dir: dir.clone(),
            shell_answer: Mutex::new(String::new()),
        });
        (
            Real {
                exec: sb.clone(),
                remote_root: None,
            },
            dir,
            sb,
        )
    }

    /// The server binary is whatever the systemd unit / launchd job executes; a package-manager symlink is refused.
    #[tokio::test]
    async fn install_detection_follows_the_service_program_and_rejects_a_symlink() {
        let (real, dir, _) = sandbox("herdr-install-detection");
        let program = dir.join("program-herdr");
        std::fs::write(&program, "regular file").unwrap();
        mock_bin(
            &dir,
            "systemctl",
            &format!(
                r#"case "$2" in is-active) echo active ;; show) printf '{{ path=%s ; argv[]=herdr server ; }}\n' {} ;; esac"#,
                sh_quote(&program.to_string_lossy())
            ),
        );
        assert_eq!(real.install_path("remote-owner").await.unwrap(), program);
        let target = dir.join("package-managed-herdr");
        std::fs::rename(&program, &target).unwrap();
        std::os::unix::fs::symlink(&target, &program).unwrap();
        let error = real.install_path("remote-owner").await.unwrap_err();
        assert!(error.contains("package manager install"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "regular file");
    }

    /// m4p：launchd 跑 `/opt/homebrew/bin/herdr`，login shell 解析到 `~/.local/bin/herdr`——不同檔要找得出來，
    /// 同一檔（含 symlink 指到同一檔）不重複，套件管理器的 symlink 拒絕。
    #[tokio::test]
    async fn the_cli_binary_is_found_when_it_is_a_different_file_from_the_server_binary() {
        let (real, dir, sb) = sandbox("herdr-cli-detection");
        let server = dir.join("server-herdr");
        let cli = dir.join("cli-herdr");
        std::fs::write(&server, "server").unwrap();
        std::fs::write(&cli, "cli").unwrap();

        *sb.shell_answer.lock().unwrap() = cli.to_string_lossy().to_string();
        assert_eq!(real.cli_path(&server).await.unwrap(), Some(cli.clone()));

        *sb.shell_answer.lock().unwrap() = server.to_string_lossy().to_string();
        assert_eq!(real.cli_path(&server).await.unwrap(), None, "同一檔不重複列");
        let alias = dir.join("alias-herdr");
        std::os::unix::fs::symlink(&server, &alias).unwrap();
        *sb.shell_answer.lock().unwrap() = alias.to_string_lossy().to_string();
        assert_eq!(real.cli_path(&server).await.unwrap(), None, "symlink 指到同一檔也算同一檔");

        let managed = dir.join("managed-herdr");
        std::os::unix::fs::symlink(&cli, &managed).unwrap();
        *sb.shell_answer.lock().unwrap() = managed.to_string_lossy().to_string();
        let error = real.cli_path(&server).await.unwrap_err();
        assert!(error.contains("package manager install") && error.contains("CLI"), "{error}");

        *sb.shell_answer.lock().unwrap() = String::new();
        assert_eq!(real.cli_path(&server).await.unwrap(), None, "找不到 CLI 就沒有第二份");
    }

    #[tokio::test]
    async fn host_scripts_swap_atomically_keep_backup_and_restore_without_any_service_calls() {
        let env = crate::testing::env().await;
        let dir = env.dir.join("host-files");
        std::fs::create_dir_all(&dir).unwrap();
        let install = dir.join("herdr ' quoted");
        let staged = dir.join("staged");
        std::fs::write(&install, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        let real = Real::local();
        let backup = real.swap(&staged, &install, "0.9.1").await.unwrap();
        assert_eq!(std::fs::read_to_string(&install).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "old");
        real.rollback(&backup, &install).await.unwrap();
        assert_eq!(std::fs::read_to_string(&install).unwrap(), "old");
        assert!(backup.exists());
        assert!(!std::fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with('.')));
        let link = dir.join("symlink");
        std::os::unix::fs::symlink(&install, &link).unwrap();
        assert!(real.swap(&staged, &link, "0.9.1").await.is_err());
        assert_eq!(std::fs::read_to_string(&install).unwrap(), "old");
    }
}
