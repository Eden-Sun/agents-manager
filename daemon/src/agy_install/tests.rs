use super::*;
use crate::testing as tt;
use sha2::{Digest, Sha512};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

const SHA_OK: &str = "fdcad6863b07ab28ab523451b17b670f1c820af40ab2b1bfead68ed927900da03fbb905cf362faec715679fbc8775331508d203373258255101d85afda9a1cb6";
const URL_OK: &str = "https://storage.googleapis.com/antigravity-public/antigravity-cli/1.3.0-6233328509124608/darwin-arm/cli_mac_arm64.tar.gz";

fn manifest_json(version: &str, url: &str, sha: &str) -> String {
    serde_json::json!({"version": version, "url": url, "sha512": sha}).to_string()
}

#[test]
fn the_official_manifest_shape_is_accepted_and_anything_else_is_not() {
    let m = parse_manifest(&manifest_json("1.3.0", URL_OK, SHA_OK)).unwrap();
    assert_eq!((m.version.as_str(), m.url.as_str(), m.sha512.as_str()), ("1.3.0", URL_OK, SHA_OK));
    assert_eq!(parse_manifest(&manifest_json("1.3.0", URL_OK, &SHA_OK.to_uppercase())).unwrap().sha512, SHA_OK, "sha512 一律小寫比對");

    let bad = |version: &str, url: &str, sha: &str| parse_manifest(&manifest_json(version, url, sha)).unwrap_err();
    // 不在官方儲存桶：不能叫主機去抓別處。
    assert!(bad("1.3.0", "https://evil.example/agy.tar.gz", SHA_OK).contains("url"));
    assert!(bad("1.3.0", "http://storage.googleapis.com/antigravity-public/antigravity-cli/x.tgz", SHA_OK).contains("url"));
    // 拼進單引號的欄位不收引號、空白、分號、$()。
    for evil in ["x'; rm -rf ~; '", "x y", "x;y", "x$(id)", "x`id`", "x\ny"] {
        assert!(bad("1.3.0", &format!("{DOWNLOAD_PREFIX}{evil}"), SHA_OK).contains("url"), "{evil:?}");
        assert!(bad(evil, URL_OK, SHA_OK).contains("version"), "{evil:?}");
    }
    assert!(bad("1.3.0", URL_OK, "abc").contains("sha512"));
    assert!(bad("1.3.0", URL_OK, &"z".repeat(128)).contains("sha512"));
    assert!(parse_manifest("not json").unwrap_err().contains("JSON"));
    assert!(parse_manifest("{}").unwrap_err().contains("version"));
}

#[test]
fn the_platform_comes_from_uname_and_libc() {
    let p = |s: &str, m: &str, musl: u8| platform_of(&format!("AM_UNAME {s} {m}\nAM_MUSL {musl}\n"));
    assert_eq!(p("Darwin", "arm64", 0), Some("darwin_arm64"));
    assert_eq!(p("Darwin", "x86_64", 0), Some("darwin_amd64"));
    assert_eq!(p("Linux", "x86_64", 0), Some("linux_amd64"));
    assert_eq!(p("Linux", "x86_64", 1), Some("linux_amd64_musl"));
    assert_eq!(p("Linux", "aarch64", 0), Some("linux_arm64"));
    assert_eq!(p("Linux", "aarch64", 1), Some("linux_arm64_musl"));
    assert_eq!(p("Linux", "riscv64", 0), None);
    assert_eq!(p("FreeBSD", "amd64", 0), None);
    assert_eq!(platform_of("nothing useful"), None);
}

#[test]
fn only_an_exact_version_counts_as_the_wanted_one() {
    assert!(has_version("1.3.0", "1.3.0"));
    assert!(has_version("agy v1.3.0", "1.3.0"));
    assert!(!has_version("11.3.01", "1.3.0"));
    assert!(!has_version("1.3.01", "1.3.0"));
    assert!(!has_version("", "1.3.0"));
}

#[test]
fn the_install_body_never_runs_the_official_installer_and_checks_the_hash_before_unpacking() {
    let body = install_body(&parse_manifest(&manifest_json("1.3.0", URL_OK, SHA_OK)).unwrap());
    assert!(!body.contains("install.sh") && !body.contains("agy install") && !body.contains("| sh") && !body.contains("| bash"), "{body}");
    let (sha_check, untar, version_check, replace) =
        (body.find("AM_AGY_SHA512_MISMATCH").unwrap(), body.find("tar -xzf").unwrap(), body.find("AM_AGY_VERSION_MISMATCH").unwrap(), body.find("mv -f").unwrap());
    assert!(sha_check < untar && untar < version_check && version_check < replace, "順序：驗 hash → 解開 → 驗版本 → 才取代");
    assert!(body.contains("AGY_CLI_DISABLE_AUTO_UPDATE=true"), "驗版本也關自動更新");
    assert!(body.contains(&format!("'{URL_OK}'")) && body.contains(&format!("'{SHA_OK}'")), "url／hash 是單引號內的資料");
}

// ── 真的跑那段 sh（假 HOME、file:// 來源）──────────────────────────────────────────────────────────

struct Fixture {
    home: PathBuf,
    tgz: PathBuf,
    sha: String,
}

fn sha512_hex(path: &Path) -> String {
    Sha512::digest(std::fs::read(path).unwrap()).iter().map(|b| format!("{b:02x}")).collect()
}

/// 一個假的 `antigravity`（shell 腳本）打成 tgz：`--version` 印 `version`，並把「當時有沒有關自動更新」記到 `seen`。
fn fixture(version: &str, member: &str) -> Fixture {
    let root = tt::scratch_dir("am-agy-install");
    let pack = root.join("pack");
    std::fs::create_dir_all(&pack).unwrap();
    let bin = pack.join(member);
    std::fs::write(&bin, format!("#!/bin/sh\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE\" >> \"{}\"\necho {version}\n", root.join("seen").display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tgz = root.join("agy.tgz");
    let st = std::process::Command::new("tar").arg("-czf").arg(&tgz).arg("-C").arg(&pack).arg(member).status().unwrap();
    assert!(st.success());
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let sha = sha512_hex(&tgz);
    Fixture { home, tgz, sha }
}

fn manifest_for(f: &Fixture, version: &str) -> Manifest {
    Manifest { version: version.into(), url: format!("file://{}", f.tgz.display()), sha512: f.sha.clone() }
}

fn run_sh(home: &Path, script: &str) -> (i32, String) {
    let tmp = home.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("HOME", home)
        .env("TMPDIR", &tmp)
        .env_remove("AGY_CLI_DISABLE_AUTO_UPDATE")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn agy_path(home: &Path) -> PathBuf {
    home.join(".local/bin/agy")
}

fn leftovers(home: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(home.join("tmp")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    out.extend(std::fs::read_dir(home.join(".local/bin")).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with(".agy.new")));
    out
}

fn seed_old_agy(home: &Path) {
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    std::fs::write(agy_path(home), "OLD").unwrap();
}

#[test]
fn macos_local_a_good_tarball_is_installed_executable_with_auto_update_off_and_nothing_left_behind() {
    let f = fixture("1.3.0", "antigravity");
    seed_old_agy(&f.home);
    let (code, out) = run_sh(&f.home, &install_body(&manifest_for(&f, "1.3.0")));
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("AM_AGY_INSTALLED {} 1.3.0", agy_path(&f.home).display())), "{out}");
    let mode = std::fs::metadata(agy_path(&f.home)).unwrap().permissions().mode();
    assert_eq!(mode & 0o755, 0o755, "{mode:o}");
    let ver = std::process::Command::new(agy_path(&f.home)).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&ver.stdout).trim(), "1.3.0", "舊的 OLD 被取代");
    let seen = std::fs::read_to_string(f.tgz.parent().unwrap().join("seen")).unwrap();
    assert!(seen.lines().next() == Some("true"), "裝之前的驗版本就帶 AGY_CLI_DISABLE_AUTO_UPDATE：{seen:?}");
    assert!(leftovers(&f.home).is_empty(), "暫存目錄與 .agy.new 都要清掉：{:?}", leftovers(&f.home));
    // 沒有動到 shell profile（官方 `agy install` 會做的事）。
    for rc in [".zshrc", ".bashrc", ".profile", ".zprofile", ".bash_profile"] {
        assert!(!f.home.join(rc).exists(), "{rc}");
    }
}

#[test]
fn macos_local_a_wrong_hash_stops_before_anything_is_unpacked_or_replaced() {
    let f = fixture("1.3.0", "antigravity");
    seed_old_agy(&f.home);
    let mut m = manifest_for(&f, "1.3.0");
    m.sha512 = "0".repeat(128);
    let (code, out) = run_sh(&f.home, &install_body(&m));
    assert_eq!(code, 73, "{out}");
    assert!(out.contains("AM_AGY_SHA512_MISMATCH"), "{out}");
    assert_eq!(std::fs::read_to_string(agy_path(&f.home)).unwrap(), "OLD", "既有的 agy 不動");
    assert!(!f.tgz.parent().unwrap().join("seen").exists(), "hash 沒過的東西不能被執行");
    assert!(leftovers(&f.home).is_empty(), "{:?}", leftovers(&f.home));
}

#[test]
fn macos_local_a_binary_that_reports_another_version_never_replaces_the_installed_one() {
    let f = fixture("1.2.9", "antigravity");
    seed_old_agy(&f.home);
    let (code, out) = run_sh(&f.home, &install_body(&manifest_for(&f, "1.3.0")));
    assert_eq!(code, 75, "{out}");
    assert!(out.contains("AM_AGY_VERSION_MISMATCH want=1.3.0 got=1.2.9"), "{out}");
    assert_eq!(std::fs::read_to_string(agy_path(&f.home)).unwrap(), "OLD");
    assert!(leftovers(&f.home).is_empty());
}

#[test]
fn macos_local_a_tarball_without_the_binary_or_an_unreachable_url_fails_cleanly() {
    let f = fixture("1.3.0", "something-else");
    let (code, out) = run_sh(&f.home, &install_body(&manifest_for(&f, "1.3.0")));
    assert_eq!(code, 74, "{out}");
    assert!(!agy_path(&f.home).exists());

    let mut m = manifest_for(&f, "1.3.0");
    m.url = "file:///nonexistent/agy.tgz".into();
    let (code, out) = run_sh(&f.home, &install_body(&m));
    assert_eq!(code, 71, "{out}");
    assert!(!agy_path(&f.home).exists());
    assert!(leftovers(&f.home).is_empty());
}

#[test]
fn macos_local_the_locked_wrapper_runs_the_body_under_the_agy_lock_and_releases_it() {
    let f = fixture("1.3.0", "antigravity");
    let script = crate::cli_update::locked_script(INSTALL_LOCK, &install_body(&manifest_for(&f, "1.3.0")));
    let (code, out) = run_sh(&f.home, &script);
    assert_eq!(code, 0, "{out}");
    assert!(installed_marker(&out).is_some_and(|(p, v)| p.ends_with(".local/bin/agy") && v == "1.3.0"), "{out}");
    assert!(!f.home.join(".agents-manager-agy-install.lock").exists(), "鎖要放掉");
    assert!(!f.home.join(".agents-manager-codex-install.lock").exists(), "不用 codex 的鎖");

    // 鎖被活著的人拿著：不裝、印 LOCKED 標記。
    let holder = std::process::Command::new("sleep").arg("30").spawn();
    let mut holder = holder.unwrap();
    let pid = holder.id();
    // 一個不是 process group 的 owner 無法驗明身份，會被當過期回收；這裡只驗「沒有 owner 資料的舊鎖會被回收而不是卡死」。
    std::os::unix::fs::symlink(pid.to_string(), f.home.join(".agents-manager-agy-install.lock")).unwrap();
    let (code, out) = run_sh(&f.home, &script);
    let _ = holder.kill();
    let _ = holder.wait();
    assert_eq!(code, 0, "舊式沒有 nonce 的鎖視為過期：{out}");
}

// ── 整條流程（假的 Env 與假 ssh）────────────────────────────────────────────────────────────────

struct FakeEnv {
    probe: String,
    manifest: Result<String, String>,
    install: Result<String, String>,
    scripts: std::sync::Mutex<Vec<String>>,
    manifests: std::sync::Mutex<Vec<String>>,
}

impl FakeEnv {
    fn new(install: Result<String, String>) -> Self {
        Self {
            probe: "AM_UNAME Darwin arm64\nAM_MUSL 0\n".into(),
            manifest: Ok(manifest_json("1.3.0", URL_OK, SHA_OK)),
            install,
            scripts: Default::default(),
            manifests: Default::default(),
        }
    }
    fn ran(&self) -> Vec<String> {
        self.scripts.lock().unwrap().clone()
    }
}

impl Env for FakeEnv {
    fn run<'a>(&'a self, _fence: &'a HostFence, script: &'a str, _timeout: Duration) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.scripts.lock().unwrap().push(script.to_string());
            if script == PLATFORM_PROBE_SH { Ok(self.probe.clone()) } else { self.install.clone() }
        })
    }
    fn manifest<'a>(&'a self, platform: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.manifests.lock().unwrap().push(platform.to_string());
            self.manifest.clone()
        })
    }
}

const DETECT_WITH_AGY: &str = "AM_PATH agy /Users/m4p/.local/bin/agy\nAM_VER agy 1.3.0\nAM_LOGIN agy 0\n";
const DETECT_WITHOUT_AGY: &str = "AM_PATH agy \nAM_LOGIN agy 0\n";

/// 一台已連線的遠端主機；ssh 的回覆（只有重新偵測會走到）由 `detect_out` 決定。
async fn remote(e: &tt::Env, detect_out: &'static str) -> String {
    let host = format!("agy-inst-{}", crate::db::ulid().to_ascii_lowercase());
    let conn = e
        .app
        .hosts
        .insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        })
        .await;
    conn.connected.store(true, Ordering::SeqCst);
    crate::hosts::set_ssh_fake(&host, move |_| Ok(detect_out.to_string()));
    host
}

async fn agy_tool(app: &Arc<App>, host: &str) -> Option<crate::tools::ToolInfo> {
    app.tools.lock().await.get(host).and_then(|h| h.tools.get("agy").cloned())
}

async fn seed_agy(app: &Arc<App>, host: &str, version: &str) {
    let ht = crate::tools::HostTools {
        tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/Users/m4p/.local/bin/agy".into()), version: Some(version.into()), logged_in: Some(true) })].into(),
        identities: Default::default(),
        shell_identities: vec![],
        utc_offset_secs: None,
        herdr_cli: None,
        checked_at: crate::db::now(),
    };
    app.tools.lock().await.insert(host.into(), ht);
}

fn ok_marker() -> Result<String, String> {
    Ok("AM_AGY_INSTALLED /Users/m4p/.local/bin/agy 1.3.0\n".into())
}

#[tokio::test]
async fn a_fresh_remote_install_downloads_the_official_build_and_redetects_the_host() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;
    let env = FakeEnv::new(ok_marker());
    let done = install_with(&e.app, &host, &env).await.expect("installed");
    assert_eq!(
        done,
        Installed {
            host: host.clone(),
            platform: "darwin_arm64".into(),
            from: None,
            to: "1.3.0".into(),
            path: Some("/Users/m4p/.local/bin/agy".into()),
            already_latest: false
        }
    );
    assert_eq!(*env.manifests.lock().unwrap(), ["darwin_arm64"], "平台由 uname 決定");
    let ran = env.ran();
    assert_eq!(ran.len(), 2, "一次平台探測＋一次安裝");
    assert!(ran[1].contains(".agents-manager-agy-install.lock") && ran[1].contains(URL_OK) && ran[1].contains(SHA_OK), "{}", ran[1]);
    assert!(!ran[1].contains("install.sh"));
    let t = agy_tool(&e.app, &host).await.expect("重新偵測後有 agy");
    assert!(t.installed && t.version.as_deref() == Some("1.3.0"), "{t:?}");
}

#[tokio::test]
async fn an_up_to_date_host_is_left_alone() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;
    seed_agy(&e.app, &host, "1.3.0").await;
    let env = FakeEnv::new(ok_marker());
    let done = install_with(&e.app, &host, &env).await.unwrap();
    assert!(done.already_latest && done.from.as_deref() == Some("1.3.0") && done.to == "1.3.0", "{done:?}");
    assert_eq!(env.ran().len(), 1, "只有平台探測，沒有下載安裝");
}

#[tokio::test]
async fn an_older_install_is_upgraded_in_place() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;
    seed_agy(&e.app, &host, "1.2.17").await;
    let env = FakeEnv::new(ok_marker());
    let done = install_with(&e.app, &host, &env).await.unwrap();
    assert!(!done.already_latest && done.from.as_deref() == Some("1.2.17") && done.to == "1.3.0", "{done:?}");
    assert_eq!(env.ran().len(), 2);
}

#[tokio::test]
async fn failures_name_their_reason_and_never_claim_success() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;
    let reason = |r: Result<Installed, InstallError>| match r {
        Err(InstallError::Failed { reason, message }) => (reason, message),
        other => panic!("expected a failure, got {other:?}"),
    };

    // 安裝腳本失敗（sha512 對不上）：訊息帶腳本的錯誤、不重新偵測、不寫 tools。
    let env = FakeEnv::new(Err("exit status: 73：AM_AGY_SHA512_MISMATCH want=a got=b".into()));
    let (r, m) = reason(install_with(&e.app, &host, &env).await);
    assert_eq!(r, "install_failed");
    assert!(m.contains("AM_AGY_SHA512_MISMATCH") && m.contains(&host), "{m}");
    assert!(agy_tool(&e.app, &host).await.is_none(), "沒裝成就不能把 agy 寫成已安裝");

    // 腳本回了成功但沒有標記：不信。
    let env = FakeEnv::new(Ok("all good\n".into()));
    assert_eq!(reason(install_with(&e.app, &host, &env).await).0, "install_failed");

    // 官方 manifest 壞掉／被換成別處的 url：不執行安裝。
    let mut env = FakeEnv::new(ok_marker());
    env.manifest = Ok(manifest_json("1.3.0", "https://evil.example/a.tgz", SHA_OK));
    assert_eq!(reason(install_with(&e.app, &host, &env).await).0, "manifest_invalid");
    assert_eq!(env.ran().len(), 1, "manifest 不合格就沒有安裝腳本");
    env.manifest = Err("官方 manifest 回 503".into());
    assert_eq!(reason(install_with(&e.app, &host, &env).await).0, "manifest_unavailable");

    // 沒有官方安裝包的平台。
    let mut env = FakeEnv::new(ok_marker());
    env.probe = "AM_UNAME Linux riscv64\nAM_MUSL 0\n".into();
    assert_eq!(reason(install_with(&e.app, &host, &env).await).0, "unsupported_platform");
    assert!(env.manifests.lock().unwrap().is_empty());

    // 寫進去了，但那台的 PATH 看不到：講清楚原因，不算成功。
    let host2 = remote(&e, DETECT_WITHOUT_AGY).await;
    let env = FakeEnv::new(ok_marker());
    let (r, m) = reason(install_with(&e.app, &host2, &env).await);
    assert_eq!(r, "not_on_path");
    assert!(m.contains("PATH") && m.contains("/Users/m4p/.local/bin/agy"), "{m}");
}

#[tokio::test]
async fn a_busy_host_refuses_a_second_install_and_a_locked_host_reports_busy() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;

    // 本行程已經有一個在跑：不開第二個、不碰主機。
    let slot = Slot::take(&host).unwrap();
    let env = FakeEnv::new(ok_marker());
    assert!(matches!(install_with(&e.app, &host, &env).await, Err(InstallError::Busy(_))));
    assert!(env.ran().is_empty());
    drop(slot);

    // 主機端的鎖被別人（隔離實例、上次沒結束的安裝）拿著：腳本印 LOCKED 標記就是 Busy，不是失敗。
    let env = FakeEnv::new(Err(format!("exit status: 75：{} pid=123", crate::cli_update::LOCKED_MARK)));
    assert!(matches!(install_with(&e.app, &host, &env).await, Err(InstallError::Busy(_))));
    assert!(Slot::take(&host).is_some(), "結束後名額要放掉");
}

#[tokio::test]
async fn unknown_and_disconnected_hosts_are_refused_before_any_command() {
    let e = tt::env().await;
    let env = FakeEnv::new(ok_marker());
    assert!(matches!(install_with(&e.app, "no-such-host", &env).await, Err(InstallError::UnknownHost)));
    let host = remote(&e, DETECT_WITH_AGY).await;
    e.app.hosts.get(&host).await.unwrap().connected.store(false, Ordering::SeqCst);
    assert!(matches!(install_with(&e.app, &host, &env).await, Err(InstallError::Failed { reason: "host_disconnected", .. })));
    assert!(env.ran().is_empty());
}

// ── 遠端登入偵測（m4p 這類遠端：憑證檔出現後 watcher 翻成已登入並探測額度）─────────────────────────

#[tokio::test]
async fn a_remote_watcher_flips_to_logged_in_when_the_token_shows_up_and_probes_the_quota() {
    let e = tt::env().await;
    let host = remote(&e, DETECT_WITH_AGY).await;
    seed_agy(&e.app, &host, "1.3.0").await;
    e.app.tools.lock().await.get_mut(&host).unwrap().tools.get_mut("agy").unwrap().logged_in = Some(false);
    let present = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let scripts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (p2, s2) = (present.clone(), scripts.clone());
    let usage = serde_json::json!({"command": {"data": {"groups": [{"name": "Gemini Models", "buckets": [
        {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.9, "reset_time": "2026-10-11T00:00:00Z"},
        {"id": "gemini-5h", "window": "five_hour", "remaining_fraction": 0.5, "reset_time": "2026-10-06T20:00:00Z"}]}]}}})
    .to_string();
    crate::hosts::set_ssh_fake(&host, move |script| {
        s2.lock().unwrap().push(script.to_string());
        if script.contains("-p /usage") {
            Ok(usage.clone())
        } else if script.contains("antigravity-oauth-token") {
            Ok(if p2.load(Ordering::SeqCst) { "AM_YES\n" } else { "AM_NO\n" }.into())
        } else {
            Ok(DETECT_WITH_AGY.into())
        }
    });

    crate::quota_agy::login_watch_once(&e.app, &host).await;
    assert_eq!(agy_tool(&e.app, &host).await.unwrap().logged_in, Some(false), "憑證檔還沒出現：不動");
    assert!(!scripts.lock().unwrap().iter().any(|s| s.contains("-p /usage")), "未登入不探測額度");

    present.store(true, Ordering::SeqCst);
    crate::quota_agy::login_watch_once(&e.app, &host).await;
    assert_eq!(agy_tool(&e.app, &host).await.unwrap().logged_in, Some(true));
    let probe = scripts.lock().unwrap().iter().find(|s| s.contains("-p /usage")).cloned().expect("登入後立刻探測一次額度");
    assert!(probe.contains("AGY_CLI_DISABLE_AUTO_UPDATE=true") && probe.contains("/Users/m4p/.local/bin/agy"), "{probe}");
    assert!(e.app.quotas.lock().await.contains_key(&format!("{host}/agy")), "遠端的讀數記在 <host>/agy");
}
