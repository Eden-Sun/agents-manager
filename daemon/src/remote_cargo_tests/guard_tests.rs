use super::*;
use std::os::unix::fs::symlink;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::time::{Duration, Instant};

pub(super) const HASH: &str = "0123456789abcdef";

pub(super) fn base() -> PathBuf {
    let d = crate::testing::track(std::env::temp_dir().join(format!("am-r141-{}", crate::db::ulid())));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 既有測試用的：名額夠多、shared 被佔著就馬上改用 job 目錄（等待另有測試）。
pub(super) const QUICK: Admission = Admission { max: 64, jobs: 1, shared_wait_secs: 0, queue_wait_secs: 30, pool: 1, try_only: false, max_load: None };

fn guard(base: &Path, hash: &str, job: &str) -> Lease {
    guard_with(base, hash, job, QUICK)
}

pub(super) fn guard_cmd(base: &Path, hash: &str, job: &str, token: &LeaseToken, adm: Admission) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(guard_script("rc", hash, job, token, adm)).current_dir(base);
    cmd
}

pub(super) fn guard_with(base: &Path, hash: &str, job: &str, adm: Admission) -> Lease {
    let token = LeaseToken::new();
    Lease::start(guard_cmd(base, hash, job, &token, adm), token, &Deadline::new(0)).expect("guard should hand out a directory")
}

fn lock_free(path: &Path) -> bool {
    Command::new("flock").arg("-n").arg(path).arg("true").status().map(|s| s.success()).unwrap_or(false)
}

/// 等待排隊中的守門送出第一聲心跳 `Q`，證明已經拿到號碼牌進入佇列。
/// 讀到 `Q` 之前不准 probe 或放鎖，避免撞上號碼牌計數競態或假綠。
fn wait_for_guard_queued(out: &mut BufReader<ChildStdout>, wait_secs: u64) {
    let until = Instant::now() + Duration::from_secs(wait_secs);
    let mut line = String::new();
    while line.trim_end() != "Q" {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "守門還沒排隊就結束了");
        assert!(!line.starts_with("ROOT"), "應該要排隊，不該拿到目錄");
        assert!(Instant::now() < until, "排隊的守門 {wait_secs} 秒內沒有心跳");
    }
}

/// 正常收尾 waiter：先關閉 stdin 讓守門走正常放鎖路徑（同 Lease::finish），再等待其結束；
/// 若超過等待上限才 kill / wait 保底，避免一開始就 kill 導致跳過釋放鎖的路徑而干擾後續測試。
fn finish_waiter(mut waiter: Child, wait_secs: u64) {
    drop(waiter.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(wait_secs.max(10));
    while waiter.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = waiter.kill();
            let _ = waiter.wait();
            panic!("waiter 關閉 stdin 後在 {wait_secs} 秒內未正常結束");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 假 cargo：記一筆「跑起來了」，然後一直跑（像一個還在編的遠端 cargo）。
fn fake_home(base: &Path) -> PathBuf {
    let home = base.join("home");
    let bin = home.join(".cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    crate::testing::write_exec(&cargo, "#!/bin/sh\ntouch \"$PWD/../cargo-started-$$\"\nexec sleep 60\n");
    home
}

fn spawn_run(base: &Path, home: &Path, dir: &str, token: &LeaseToken) -> Child {
    // 遠端每條 ssh 都是自己的 session（sshd setsid）；這裡用獨立的 process group 模擬，收 pgid 才不會打到測試本身。
    Command::new("sh")
        .arg("-c")
        .arg(run_script(dir, "", token, 1, 0, &["test".into()]))
        .env("HOME", home)
        .current_dir(base)
        .process_group(0)
        .spawn()
        .unwrap()
}

fn started(dir: &str) -> bool {
    let parent = Path::new(dir).parent().unwrap();
    std::fs::read_dir(parent)
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("cargo-started-")))
        .unwrap_or(false)
}

/// 結束後自己的 job 目錄不在了；shared 留著給下一次（target 還在），而且被佔用時第二個呼叫拿不到它。
#[test]
fn a_finished_call_removes_its_private_dir_and_keeps_the_shared_target() {
    let base = base();
    let mut a = guard(&base, HASH, "job-1-1");
    assert!(a.hs.dir.ends_with(&format!("/{HASH}/shared")), "{:?}", a.hs);
    std::fs::create_dir_all(Path::new(&a.hs.dir).join("target")).unwrap();
    std::fs::write(Path::new(&a.hs.dir).join("target/marker"), "x").unwrap();

    let mut b = guard(&base, HASH, "job-2-2");
    assert!(b.hs.dir.ends_with(&format!("/{HASH}/job-2-2")), "shared 被 a 佔著，b 要退回自己的目錄：{:?}", b.hs);
    let bdir = b.hs.dir.clone();
    assert!(Path::new(&bdir).is_dir());
    assert!(b.finish().unwrap().success());
    assert!(!Path::new(&bdir).exists(), "job 目錄結束後要刪掉");
    assert!(!Path::new(&format!("{bdir}.lock")).exists());

    let adir = a.hs.dir.clone();
    assert!(a.finish().unwrap().success());
    assert!(Path::new(&adir).join("target/marker").is_file(), "shared 的 target 要留給下一次");

    let mut c = guard(&base, HASH, "job-3-3");
    assert_eq!(c.hs.dir, adir, "shared 還回去之後，下一次要重用它");
    c.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 被中斷（helper／ssh 死掉，遠端只看到 stdin EOF）：還在跑的遠端 cargo 整組收掉，目錄刪掉。
#[test]
fn an_interrupted_call_stops_the_remote_cargo_and_removes_its_dir() {
    let base = base();
    let home = fake_home(&base);
    let mut holder = guard(&base, HASH, "job-1-1"); // 佔住 shared，讓下一個拿 job 目錄
    let mut lease = guard(&base, HASH, "job-2-2");
    let dir = lease.hs.dir.clone();
    let mut run = spawn_run(&base, &home, &dir, &lease.token);
    wait_until("fake cargo to start", || started(&dir));

    drop(lease.stdin.take()); // 本機那端沒了
    assert!(lease.finish().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(st) = run.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = run.kill();
            panic!("remote cargo still running after its lease ended");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.signal().is_some() || !status.success(), "{status:?}");
    assert!(!Path::new(&dir).exists(), "中斷也要刪目錄");
    holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// helper 已經死了、目錄也還回去了，才姍姍來遲的 run（孤兒 ssh）不能在別人的目錄裡跑 cargo。
#[test]
fn a_late_run_after_the_lease_ended_does_not_start_cargo() {
    let base = base();
    let home = fake_home(&base);
    let mut lease = guard(&base, HASH, "job-1-1");
    let (dir, token) = (lease.hs.dir.clone(), lease.token.clone());
    lease.finish();
    let status = spawn_run(&base, &home, &dir, &token).wait().unwrap();
    assert_eq!(status.code(), Some(126));
    assert!(!started(&dir));
    let _ = std::fs::remove_dir_all(base);
}

/// #177：從子目錄呼叫時，遠端也要在同一個子目錄下跑 cargo（相對路徑的參數才跟本機是同一個意思）；
/// 就在根的呼叫照舊在根目錄跑。
#[test]
fn the_remote_run_starts_in_the_same_subdirectory_as_the_local_call() {
    let base = base();
    let home = fake_home(&base);
    // 這裡的假 cargo 把自己的 cwd 記下來就結束。
    let cargo = home.join(".cargo/bin/cargo");
    std::fs::write(&cargo, "#!/bin/sh\npwd > \"$AM_TEST_PWD\"\n").unwrap();
    let mut lease = guard(&base, HASH, "job-1-1");
    let dir = lease.hs.dir.clone();
    std::fs::create_dir_all(Path::new(&dir).join("daemon/src")).unwrap();
    for sub in ["daemon/src", ""] {
        let out = base.join("pwd.out");
        let _ = std::fs::remove_file(&out);
        let status = Command::new("sh")
            .arg("-c")
            .arg(run_script(&dir, sub, &lease.token, 1, 0, &["check".into()]))
            .env("HOME", &home)
            .env("AM_TEST_PWD", &out)
            .current_dir(&base)
            .status()
            .unwrap();
        assert!(status.success(), "{sub}: {status:?}");
        let cwd = std::fs::read_to_string(&out).unwrap();
        let want = Path::new(&dir).join(sub);
        assert_eq!(std::fs::canonicalize(cwd.trim()).unwrap(), std::fs::canonicalize(&want).unwrap(), "sub={sub:?}");
    }
    lease.finish();
    let _ = std::fs::remove_dir_all(base);
}

fn wait_run(run: &mut Child) -> ExitStatus {
    // 等待 run 結束；若 stale run 違規啟動 cargo 則不會主動退出，故需以牆鐘 deadline 防護避免無窮卡死
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(st) = run.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = unsafe { libc::kill(-(run.id() as i32), libc::SIGKILL) };
            let _ = run.wait();
            panic!("the stale run went on to start cargo");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// #148：租約身份不能是遠端 PID。上一代結束、下一代租到同一個 shared 之後，姍姍來遲的舊 run
/// （帶的是上一代的 token）不能通過 owner 檢查；下一代自己的 run 照常跑。
/// 修正前 token 是守門的 `$$`，兩代剛好同一個 remote PID 時 `.owner` 又變回舊值、舊 run 誤通過
/// （當時用把 `$$` 釘成同一個數字模擬，紅在「the stale run went on to start cargo」）。
#[test]
fn a_stale_run_is_refused_after_the_next_lease_takes_over_the_same_dir() {
    let base = base();
    let home = fake_home(&base);
    let mut a = guard(&base, HASH, "job-1-1");
    let (dir, stale) = (a.hs.dir.clone(), a.token.clone());
    a.finish();
    let mut b = guard(&base, HASH, "job-2-2");
    assert_eq!(b.hs.dir, dir, "b 要租到同一個 shared");
    assert_ne!(b.token, stale, "每一代都要有新的 token");

    let status = wait_run(&mut spawn_run(&base, &home, &dir, &stale));
    assert_eq!(status.code(), Some(126), "{status:?}");
    assert!(!started(&dir), "舊 run 不能在下一代的目錄裡跑 cargo");

    // 同一次租約的 run 照常跑。
    let mut run = spawn_run(&base, &home, &dir, &b.token);
    wait_until("the current lease's cargo to start", || started(&dir));
    assert!(b.finish().unwrap().success());
    wait_run(&mut run);
    let _ = std::fs::remove_dir_all(base);
}

/// `.owner`、pgid sidecar 與交給 run 的 token 都是本機給的那個，不是守門 shell 的 PID；
/// 下一代結束時只收自己那一代的 sidecar，舊 run 留下的別人的 sidecar 不當成自己的去殺。
#[test]
fn the_lease_identity_is_the_callers_token_and_cleanup_only_reaps_its_own_generation() {
    let base = base();
    let home = fake_home(&base);
    let mut a = guard(&base, HASH, "job-1-1");
    let (dir, stale) = (a.hs.dir.clone(), a.token.clone());
    assert_eq!(a.hs.token, stale.as_str(), "守門要把 token 原樣交回來");
    assert_eq!(std::fs::read_to_string(format!("{dir}.owner")).unwrap().trim(), stale.as_str());
    assert_ne!(stale.as_str(), a.child.id().to_string(), "身份不能是守門 shell 的 PID");
    a.finish();

    let mut b = guard(&base, HASH, "job-2-2");
    assert_eq!(std::fs::read_to_string(format!("{dir}.owner")).unwrap().trim(), b.token.as_str());
    // 舊 run 被拒之前已經寫下的 sidecar（指向一個不屬於這一代的 process group）。
    let mut decoy = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
    std::fs::write(format!("{dir}.pgid-{}", stale.as_str()), format!("{}\n", decoy.id())).unwrap();
    let mut run = spawn_run(&base, &home, &dir, &b.token);
    wait_until("the current lease's cargo to start", || started(&dir));
    assert!(Path::new(&format!("{dir}.pgid-{}", b.token.as_str())).is_file(), "sidecar 用 token 命名");

    assert!(b.finish().unwrap().success());
    wait_run(&mut run);
    assert!(decoy.try_wait().unwrap().is_none(), "別人那一代的 sidecar 指到的行程不能被收");
    let _ = decoy.kill();
    let _ = decoy.wait();
    let left: Vec<_> = std::fs::read_dir(Path::new(&dir).parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".pgid-"))
        .collect();
    assert!(left.is_empty(), "結束後 sidecar 要清乾淨：{left:?}");
    let _ = std::fs::remove_dir_all(base);
}

/// 回收只刪沒人在用的孤兒：有行程 cwd 在裡面、鎖被持有、不是自己命名規則的、根目錄外的、
/// 自己這次的，就算被點名也不刪（守門自己再看一次，不只相信 helper 的篩選）。
#[test]
fn gc_removes_orphans_but_never_a_dir_in_use_or_outside_its_layout() {
    let base = base();
    let rc = base.join("rc");
    let (h, h2) = ("00000000000000aa", "00000000000000bb");
    for d in [format!("{h}/111"), format!("{h}/222"), format!("{h}/333"), format!("{h}/src"), format!("{h2}/444"), "nothex/555".into()] {
        std::fs::create_dir_all(rc.join(d)).unwrap();
    }
    std::fs::create_dir_all(base.join("outside")).unwrap();
    let mut busy = Command::new("sleep").arg("60").current_dir(rc.join(h).join("222")).spawn().unwrap();
    let lock = rc.join(h).join("333.lock");
    let mut locker = Command::new("flock")
        .arg(&lock)
        .arg("sh")
        .arg("-c")
        .arg("echo LOCKED; exec sleep 60")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut locker_out = BufReader::new(locker.stdout.take().unwrap());
    let mut locked_line = String::new();
    assert!(locker_out.read_line(&mut locked_line).unwrap() > 0, "locker 未取得鎖就結束了");
    assert_eq!(locked_line.trim(), "LOCKED");

    let mut g = guard(&base, HASH, "job-9-9");
    let root = g.hs.root.clone();
    let find = |leaf: &str| g.hs.dirs.iter().find(|d| d.path == format!("{root}/{leaf}")).cloned();
    let orphan = find(&format!("{h}/111")).expect("orphan listed");
    assert!(!orphan.locked && !orphan.busy, "{orphan:?}");
    assert!(find(&format!("{h}/222")).unwrap().busy);
    assert!(find(&format!("{h}/333")).unwrap().locked);
    assert!(find(&format!("{h}/src")).is_none() && find("nothex/555").is_none(), "{:?}", g.hs.dirs);
    assert!(select_gc(&g.hs, &GcPolicy::default()).is_empty(), "剛建的目錄還沒到閒置門檻：{:?}", g.hs.dirs);

    let own = g.hs.dir.clone();
    let named: Vec<String> = [
        format!("{root}/{h}/111"),
        format!("{root}/{h}/222"),
        format!("{root}/{h}/333"),
        format!("{root}/{h}/src"),
        format!("{root}/{h2}/444"),
        format!("{root}/nothex/555"),
        format!("{root}/{h}/../{h2}"),
        base.join("outside").to_string_lossy().into_owned(),
        own.clone(),
    ]
    .into();
    g.collect(&named);
    assert!(g.finish().unwrap().success());

    assert!(!rc.join(h).join("111").exists(), "閒置孤兒要收");
    assert!(!rc.join(h2).exists(), "收完變空的 hash 目錄一起收");
    assert!(rc.join(h).join("222").is_dir(), "有行程在用的不能刪");
    assert!(rc.join(h).join("333").is_dir(), "鎖被持有的不能刪");
    assert!(rc.join(h).join("src").is_dir() && rc.join("nothex/555").is_dir(), "不是自己命名的不能刪");
    assert!(base.join("outside").is_dir(), "根目錄外的不能刪");
    assert!(Path::new(&own).is_dir(), "自己這次的 shared 不能被自己回收");

    let _ = busy.kill();
    let _ = locker.kill();
    let _ = busy.wait();
    let _ = locker.wait();
    let _ = std::fs::remove_dir_all(base);
}

// ── 整體執行上限（issue #194）：用假的 ssh／rsync 把本機當成遠端，跑真的 `run_offload`＋真的守門腳本 ──

/// 寫一支假腳本（`body` 接在 `#!/bin/sh` 之後）並確定它已經可以被 exec（issue #189）：見 [`crate::testing::write_exec`]。
pub(super) fn write_exec(path: &Path, body: &str) {
    crate::testing::write_exec(path, format!("#!/bin/sh\n{body}"))
}

/// 假遠端：`ssh` 把最後一個參數（要在遠端跑的指令字串）在**本機**用 `sh -c` 跑，跟 sshd 一樣每條連線自成一個 session
/// （`setsid`；收 process group 才不會打到測試本身）；`rsync` 把來源目錄複製到 `user@host:` 後面那個路徑。
/// `$HOME` 指到沙盒，裡面的假 cargo 就是「遠端的 cargo」。回傳 (設定, cwd, data_dir)。
pub(super) fn fake_remote(base: &Path, timeout_secs: u64, cargo_body: &str) -> (BuildRemoteCfg, PathBuf, PathBuf) {
    let bin = base.join("fakebin");
    let home = base.join("home");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
    write_exec(&home.join(".cargo/bin/cargo"), cargo_body);
    write_exec(
        &bin.join("ssh"),
        &format!("for a; do last=$a; done\nHOME='{}' exec setsid sh -c \"$last\"\n", home.display()),
    );
    write_exec(&bin.join("rsync"), "for a; do src=$dest; dest=$a; done\nd=${dest#*:}\nmkdir -p \"$d\" && cp -a \"${src}.\" \"$d\"\n");
    let cwd = base.join("work");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let remote = BuildRemoteCfg {
        enabled: true,
        host: "fake".into(),
        user: "me".into(),
        ssh_port: 22,
        remote_root: base.join("rc").display().to_string(),
        cargo_jobs: 1,
        test_threads: 0,
        timeout_secs,
        ..Default::default()
    };
    (remote, cwd, data)
}

/// 這條執行緒上的等待與負載都密封住再跑 `f`：shared 等待只等 [`SEALED_SHARED_WAIT_SECS`] 秒，負載檔是低負載（`base/loadavg`）。
/// 巢狀安全：結束時還原外層設的值。要測長等待的測試在 `f` 裡自己覆寫 [`TEST_SHARED_WAIT_SECS`]。
pub(super) fn with_sealed_waits<T>(base: &Path, f: impl FnOnce() -> T) -> T {
    let file = base.join("loadavg");
    std::fs::write(&file, "0.10 0.10 0.10 1/1 1\n").unwrap();
    let prev_wait = TEST_SHARED_WAIT_SECS.with(|w| w.replace(Some(SEALED_SHARED_WAIT_SECS)));
    let prev_load = TEST_LOADAVG.with(|p| p.replace(Some(file)));
    let out = f();
    TEST_SHARED_WAIT_SECS.with(|w| w.set(prev_wait));
    TEST_LOADAVG.with(|p| *p.borrow_mut() = prev_load);
    out
}

/// 測試預設的 shared 等待上限（秒）：正式是 [`SHARED_WAIT_SECS`] 的 900 秒，測試只給 10 秒。
pub(super) const SEALED_SHARED_WAIT_SECS: u64 = 10;

/// 在這條執行緒上把 `ssh`／`rsync` 換成 `fake_remote` 寫的假腳本、等待與負載密封住，再跑 `f`。
pub(super) fn with_fake_transport<T>(base: &Path, f: impl FnOnce() -> T) -> T {
    let bin = base.join("fakebin");
    TEST_PROGRAMS.with(|p| *p.borrow_mut() = Some((bin.join("ssh").display().to_string(), bin.join("rsync").display().to_string())));
    let out = with_sealed_waits(base, f);
    TEST_PROGRAMS.with(|p| *p.borrow_mut() = None);
    out
}

/// 接上本機真 rsync，並要求生產程式傳入快取安全旗標。
fn use_real_rsync_for_fake_remote(base: &Path) {
    write_exec(
        &base.join("fakebin/rsync"),
        r#"src=; dest=; skip=; checksum=; no_times=; exclude_count=0; e1=; e2=; e3=; e4=; e5=; e6=; e7=; e8=
for a; do
  if [ "$skip" = exclude ]; then
    exclude_count=$((exclude_count + 1))
    case "$exclude_count" in 1) e1=$a ;; 2) e2=$a ;; 3) e3=$a ;; 4) e4=$a ;; 5) e5=$a ;; 6) e6=$a ;; 7) e7=$a ;; 8) e8=$a ;; *) echo 'too many excludes' >&2; exit 98 ;; esac
    skip=; continue
  fi
  if [ "$skip" = value ]; then skip=; continue; fi
  case "$a" in
    --checksum) checksum=1 ;;
    --no-times) no_times=1 ;;
    --exclude) skip=exclude ;;
    -e) skip=value ;;
    -az|--delete|--quiet) ;;
    -*) ;;
    *) if [ -z "$src" ]; then src=$a; else dest=$a; fi ;;
  esac
done
[ -n "$checksum" ] && [ -n "$no_times" ] || { echo 'missing cache safety flags' >&2; exit 97; }
d=${dest#*:}
set -- -az --delete --checksum --no-times --quiet
for e in "$e1" "$e2" "$e3" "$e4" "$e5" "$e6" "$e7" "$e8"; do [ -z "$e" ] || set -- "$@" --exclude "$e"; done
set -- "$@" "$src" "$d"
rsync "$@"; rc=$?
fail_marker="${src%/}/../.rsync-fail-after-copy"
if [ -f "$fail_marker" ]; then
  n=$(cat "$fail_marker"); case "$n" in ''|*[!0-9]*) n=0 ;; esac
  if [ "$n" -gt 0 ]; then n=$((n - 1)); if [ "$n" -eq 0 ]; then rm -f "$fail_marker"; else printf '%s\n' "$n" > "$fail_marker"; fi; echo 'rsync: vanished file: No such file or directory (2)' >&2; exit 24; fi
fi
exit "$rc"
"#,
    );
}

/// node_modules 只有專案明確選擇排除時才不傳；切換政策時也清掉遠端舊值。
#[test]
fn remote_cargo_exclusions_are_opt_in_and_prune_old_remote_inputs() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        cwd.join("Cargo.toml"),
        "[workspace]\n[workspace.metadata.agents-manager.remote-cargo]\nexclude = [\"/web/node_modules\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(cwd.join("web/node_modules")).unwrap();
    std::fs::write(cwd.join("web/node_modules/package.json"), "current\n").unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(Path::new(&dir).join("web/node_modules")).unwrap();
    std::fs::write(Path::new(&dir).join("web/node_modules/stale.json"), "old\n").unwrap();
    use_real_rsync_for_fake_remote(&base);

    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("explicit node_modules exclusion should sync");
    assert!(
        !Path::new(&dir).join("web/node_modules").exists(),
        "excluded paths must not leave old remote inputs behind"
    );

    std::fs::write(cwd.join("Cargo.toml"), "[workspace]\n").unwrap();
    sync().expect("default policy should preserve node_modules");
    assert_eq!(
        std::fs::read(Path::new(&dir).join("web/node_modules/package.json")).unwrap(),
        b"current\n"
    );
    let _ = std::fs::remove_dir_all(base);
}

/// #1861 review：遠端殘留的根 `.claude`（worktree 與其 target 可能十幾 GB）不能被整棵 touch 遍歷；一般原始碼照樣 touch。
#[test]
fn the_remote_touch_skips_a_leftover_root_claude_tree() {
    let base = base();
    let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
    let dir = base.join("shared");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let stale = dir.join(".claude/worktrees/x/target/big");
    let stale_dist = dir.join("web/dist/index.html");
    let live = dir.join("src/lib.rs");
    for p in [&stale, &stale_dist, &live] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
        std::fs::File::options().write(true).open(p).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    }
    with_fake_transport(&base, || touch_remote_workspace(&remote, &data, &dir.display().to_string(), &Deadline::new(60))).expect("touch succeeds");
    assert_eq!(std::fs::metadata(&stale).unwrap().modified().unwrap(), old, "遠端殘留的 .claude 不被 touch");
    assert!(std::fs::metadata(&stale_dist).unwrap().modified().unwrap() > old, "workspace touch includes frontend build inputs");
    assert!(std::fs::metadata(&live).unwrap().modified().unwrap() > old, "一般原始碼照樣 touch");

    std::fs::File::options().write(true).open(&live).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    with_fake_transport(&base, || {
        touch_remote_files(
            &remote,
            &data,
            &dir.display().to_string(),
            &[
                PathBuf::from(".claude/worktrees/x/target/big"),
                PathBuf::from("web/dist/index.html"),
                PathBuf::from("src/lib.rs"),
            ],
            &Deadline::new(60),
        )
    })
    .expect("individual touch must skip the excluded root .claude tree");
    assert_eq!(std::fs::metadata(&stale).unwrap().modified().unwrap(), old, "逐檔 touch 也略過根 .claude");
    assert!(std::fs::metadata(&stale_dist).unwrap().modified().unwrap() > old, "individual touch includes frontend build inputs");
    assert!(std::fs::metadata(&live).unwrap().modified().unwrap() > old, "逐檔 touch 仍更新一般原始碼");
    let _ = std::fs::remove_dir_all(&base);
}

/// Exclusion cleanup must fail closed when an intermediate remote path is a symlink.
#[test]
fn remote_exclusion_pruning_refuses_symlink_ancestors() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        cwd.join("Cargo.toml"),
        "[workspace]\n[workspace.metadata.agents-manager.remote-cargo]\nexclude = [\"/web/node_modules\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(cwd.join("web/node_modules")).unwrap();
    std::fs::write(cwd.join("web/node_modules/package.json"), "source\n").unwrap();

    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    let outside = base.join("outside");
    std::fs::create_dir_all(outside.join("node_modules")).unwrap();
    std::fs::write(outside.join("node_modules/keep.json"), "must survive\n").unwrap();
    std::fs::create_dir_all(Path::new(&dir)).unwrap();
    symlink(&outside, Path::new(&dir).join("web")).unwrap();
    use_real_rsync_for_fake_remote(&base);

    let result = with_fake_transport(&base, || {
        sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
    });
    assert!(result.is_err(), "an excluded path behind a symlink ancestor must be refused");
    assert_eq!(
        std::fs::read(outside.join("node_modules/keep.json")).unwrap(),
        b"must survive\n",
        "cleanup must not traverse outside the leased directory"
    );
    assert!(std::fs::symlink_metadata(Path::new(&dir).join("web")).unwrap().file_type().is_symlink());
    let _ = std::fs::remove_dir_all(base);
}

/// Removed parents and directory-to-file transitions must not be recreated or passed to touch.
#[test]
fn workspace_invalidation_ignores_removed_paths_and_prunes_target() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::create_dir_all(cwd.join("removed/child")).unwrap();
    std::fs::write(cwd.join("removed/child/input.txt"), "old\n").unwrap();
    std::fs::create_dir_all(cwd.join("switched/child")).unwrap();
    std::fs::write(cwd.join("switched/child/input.txt"), "old\n").unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(Path::new(&dir)).unwrap();
    use_real_rsync_for_fake_remote(&base);

    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("initial tree sync should succeed");
    let target_file = Path::new(&dir).join("target/debug/sentinel");
    std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
    std::fs::write(&target_file, "keep target\n").unwrap();
    // 固定在過去的時間：任何會動到 target 的同步都會把 mtime 改成現在，不必等檔案系統的秒數精度。
    set_mtime(&target_file, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let target_mtime = std::fs::metadata(&target_file).unwrap().modified().unwrap();

    std::fs::remove_dir_all(cwd.join("removed")).unwrap();
    std::fs::remove_dir_all(cwd.join("switched")).unwrap();
    std::fs::write(cwd.join("switched"), "now a regular file\n").unwrap();
    sync().expect("deletion and directory-to-file transition should synchronize");

    assert!(!Path::new(&dir).join("removed").exists(), "deleted paths must not be recreated by invalidation");
    assert_eq!(std::fs::read(Path::new(&dir).join("switched")).unwrap(), b"now a regular file\n");
    assert_eq!(std::fs::metadata(&target_file).unwrap().modified().unwrap(), target_mtime, "workspace invalidation must leave target artifacts alone");
    let _ = std::fs::remove_dir_all(base);
}

fn source_manifest(entries: &[(&str, u8, &[u8], u32)]) -> SourceManifest {
    SourceManifest {
        entries: entries
            .iter()
            .map(|(path, kind, value, mode)| {
                (
                    path.as_bytes().to_vec(),
                    SourceEntry {
                        kind: *kind,
                        mode: *mode,
                        value: value.to_vec(),
                    },
                )
            })
            .collect(),
    }
}

const TEST_WEB_EMBED_SOURCE: &str = r#"#[cfg(feature = "embed-ui")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct WebAssets;

use std::collections::BTreeMap;

#[cfg(feature = "embed-ui")]
pub fn files() -> BTreeMap<String, Vec<u8>> {
    WebAssets::iter()
        .map(|path| {
            let path = path.to_string();
            let data = WebAssets::get(&path).expect("embedded path").data.into_owned();
            (path, data)
        })
        .collect()
}

#[cfg(not(feature = "embed-ui"))]
pub fn files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::new()
}
"#;
const ACTUAL_WEB_EMBED_SOURCE: &str = include_str!("../../../crates/am-base/src/assets.rs");

fn install_test_web_embed_anchor(root: &Path) {
    let path = root.join(WEB_DIST_EMBED_ANCHOR);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, ACTUAL_WEB_EMBED_SOURCE).unwrap();
}

#[test]
fn source_manifest_only_narrows_safe_regular_file_changes() {
    let root = Path::new("/workspace");
    let previous = source_manifest(&[
        ("Cargo.toml", b'F', b"manifest", 0o644),
        ("src/lib.rs", b'F', b"old", 0o644),
        ("src/other.rs", b'F', b"same", 0o644),
    ]);
    let changed = source_manifest(&[
        ("Cargo.toml", b'F', b"manifest", 0o644),
        ("src/lib.rs", b'F', b"new", 0o644),
        ("src/other.rs", b'F', b"same", 0o644),
    ]);
    assert_eq!(
        source_invalidation(root, Some(&previous), &changed),
        SourceInvalidation::Files(vec![PathBuf::from("src/lib.rs")]),
        "an unchanged-type Rust file can be invalidated by itself"
    );
    assert_eq!(
        source_invalidation(root, Some(&previous), &previous),
        SourceInvalidation::Unchanged,
        "an unchanged source tree must keep the hot target intact"
    );

    let mode_changed = source_manifest(&[
        ("Cargo.toml", b'F', b"manifest", 0o644),
        ("src/lib.rs", b'F', b"old", 0o755),
        ("src/other.rs", b'F', b"same", 0o644),
    ]);
    assert_eq!(
        source_invalidation(root, Some(&previous), &mode_changed),
        SourceInvalidation::Files(vec![PathBuf::from("src/lib.rs")]),
        "a permission-only change to a regular file can be invalidated by itself"
    );

    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        ".cargo/config.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
        "crates/core/Cargo.toml",
        "crates/core/build.rs",
        "crates/core/rust-toolchain.toml",
    ] {
        let before = source_manifest(&[(path, b'F', b"old", 0o644)]);
        let after = source_manifest(&[(path, b'F', b"new", 0o644)]);
        assert_eq!(
            source_invalidation(root, Some(&before), &after),
            SourceInvalidation::Workspace,
            "Cargo control input {path:?} must invalidate the full workspace"
        );
        let added = source_manifest(&[(path, b'F', b"new", 0o644)]);
        let removed = source_manifest(&[]);
        assert_eq!(
            source_invalidation(root, Some(&removed), &added),
            SourceInvalidation::Workspace,
            "adding Cargo control input {path:?} must invalidate the full workspace"
        );
        assert_eq!(
            source_invalidation(root, Some(&added), &removed),
            SourceInvalidation::Workspace,
            "removing Cargo control input {path:?} must invalidate the full workspace"
        );
    }

    let before = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"old", 0o644),
        ("src/removed.rs", b'F', b"old", 0o644),
    ]);
    let added_and_removed = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"old", 0o644),
        ("src/new.txt", b'F', b"new", 0o644),
    ]);
    let directory_changed = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/new-dir", b'D', b"", 0o755),
    ]);
    let type_changed = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'L', b"target.rs", 0o777),
    ]);
    assert_eq!(
        source_invalidation(root, Some(&before), &added_and_removed),
        SourceInvalidation::Workspace,
        "even non-Rust additions may change untracked compile-time directory listings"
    );
    assert_eq!(
        source_invalidation(root, Some(&before), &directory_changed),
        SourceInvalidation::Files(vec![PathBuf::from("src")]),
        "an added empty directory needs no touch; removed files touch their surviving parent"
    );
    assert_eq!(
        source_invalidation(root, Some(&before), &type_changed),
        SourceInvalidation::Workspace,
        "type changes must conservatively invalidate the workspace"
    );
    assert_eq!(
        source_invalidation(root, None, &before),
        SourceInvalidation::Workspace,
        "missing or corrupt baselines must invalidate the workspace"
    );

    let existing_module_pair = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"mod foo;", 0o644),
        ("src/foo", b'D', b"", 0o755),
        ("src/foo/mod.rs", b'F', b"", 0o644),
    ]);
    let ambiguous_module = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"mod foo;", 0o644),
        ("src/foo", b'D', b"", 0o755),
        ("src/foo/mod.rs", b'F', b"", 0o644),
        ("src/foo.rs", b'F', b"", 0o644),
    ]);
    let report = source_invalidation_report(root, Some(&existing_module_pair), &ambiguous_module);
    assert_eq!(report.action, SourceInvalidation::Workspace);
    assert_eq!(
        report.fallback["file added"].count,
        1,
        "a new Rust module candidate with no owning package cannot be narrowed"
    );

    // With an owning package, the new module candidate touches that crate's roots only.
    let in_crate = |extra: &[(&'static str, u8, &'static [u8], u32)]| {
        let mut entries = vec![
            ("crates/core", b'D', &b""[..], 0o755),
            ("crates/core/Cargo.toml", b'F', &b"[package]"[..], 0o644),
            ("crates/core/src", b'D', &b""[..], 0o755),
            ("crates/core/src/lib.rs", b'F', &b"mod foo;"[..], 0o644),
            ("crates/core/src/foo", b'D', &b""[..], 0o755),
            ("crates/core/src/foo/mod.rs", b'F', &b""[..], 0o644),
            ("crates/other", b'D', &b""[..], 0o755),
            ("crates/other/Cargo.toml", b'F', &b"[package]"[..], 0o644),
            ("crates/other/src", b'D', &b""[..], 0o755),
            ("crates/other/src/main.rs", b'F', &b""[..], 0o644),
        ];
        entries.extend_from_slice(extra);
        source_manifest(&entries)
    };
    let report = source_invalidation_report(root, Some(&in_crate(&[])), &in_crate(&[("crates/core/src/foo.rs", b'F', b"", 0o644)]));
    assert_eq!(
        report.action,
        SourceInvalidation::Files(vec![PathBuf::from("crates/core/src/foo.rs"), PathBuf::from("crates/core/src/lib.rs")]),
        "only the owning crate recompiles; crates/other is untouched"
    );
    assert_eq!(report.targeted["Rust source added"], 1);
    let report = source_invalidation_report(
        root,
        Some(&in_crate(&[])),
        &in_crate(&[("crates/core/tests", b'D', b"", 0o755), ("crates/core/tests/new_case.rs", b'F', b"", 0o644)]),
    );
    assert_eq!(
        report.action,
        SourceInvalidation::Files(vec![PathBuf::from("crates/core/src/lib.rs"), PathBuf::from("crates/core/tests/new_case.rs")]),
        "a new directory with a new test file stays narrowed to its crate"
    );
}

#[test]
fn workspace_root_permission_changes_are_manifest_inputs() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = base();
    let root = base.join("source");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = scan_source_manifest(&root, &[]).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
    let after = scan_source_manifest(&root, &[]).unwrap();
    assert_eq!(source_invalidation(&root, Some(&before), &after), SourceInvalidation::Workspace);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn source_manifest_report_contains_only_categories_and_counts() {
    let root = Path::new("/workspace");
    let before = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"old", 0o644),
        ("src/removed-dir", b'D', b"", 0o755),
        ("Cargo.toml", b'F', b"old", 0o644),
    ]);
    let after = source_manifest(&[
        ("src", b'D', b"", 0o755),
        ("src/added-dir", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"new", 0o644),
        ("Cargo.toml", b'F', b"new", 0o644),
    ]);

    let report = source_invalidation_report(root, Some(&before), &after);
    assert_eq!(report.action, SourceInvalidation::Workspace);
    assert_eq!(report.safe_file_count, 1);
    assert_eq!(report.targeted["directory added"], 1);
    assert!(!report.fallback.contains_key("added directory"));
    assert_eq!(report.fallback["removed directory"].count, 1);
    assert_eq!(report.fallback["Cargo control input changed"].count, 1);
    let diagnostic = report.diagnostic().unwrap();
    assert!(diagnostic.contains("removed directory (1)"));
    assert!(diagnostic.contains("targeted categories: directory added (1)"));
    assert!(diagnostic.contains("1 safe regular-file changes"));
    assert!(!diagnostic.contains("src/lib.rs"));
    assert!(!diagnostic.contains("Cargo.toml"));
    assert!(!diagnostic.contains("old"));
    let added_control = source_invalidation_report(
        root,
        Some(&source_manifest(&[])),
        &source_manifest(&[("rust-toolchain.toml", b'F', b"stable", 0o644)]),
    );
    assert_eq!(added_control.fallback["Cargo control input added"].count, 1);
}

#[test]
fn web_dist_structural_changes_use_verified_embed_anchor_and_union_safe_files() {
    let base = base();
    let root = base.join("source");
    install_test_web_embed_anchor(&root);
    let anchor = (WEB_DIST_EMBED_ANCHOR, b'F', ACTUAL_WEB_EMBED_SOURCE.as_bytes(), 0o644);
    let before = source_manifest(&[
        anchor,
        ("web", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"old", 0o644),
    ]);
    let after = source_manifest(&[
        anchor,
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/index.html", b'F', b"stub", 0o644),
        ("web/dist/assets", b'D', b"", 0o755),
        ("web/dist/assets/new-hash.js", b'F', b"new frontend", 0o644),
        ("src/lib.rs", b'F', b"new", 0o644),
    ]);

    let report = source_invalidation_report(&root, Some(&before), &after);
    assert_eq!(
        report.action,
        SourceInvalidation::Files(vec![
            PathBuf::from(WEB_DIST_EMBED_ANCHOR),
            PathBuf::from("src/lib.rs"),
        ]),
        "new web output must touch the real embed consumer and union ordinary safe changes"
    );
    let diagnostic = report.diagnostic().unwrap();
    assert!(diagnostic.contains("embedded UI entry added (4)"));
    assert!(diagnostic.contains("safe regular-file changes (1)"));
    assert!(!diagnostic.contains(WEB_DIST_EMBED_ANCHOR));
    assert!(!diagnostic.contains("src/lib.rs"));
    assert!(!diagnostic.contains("new frontend"));

    let removed = source_manifest(&[
        anchor,
        ("web", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"newer", 0o644),
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&after), &removed).action,
        SourceInvalidation::Files(vec![
            PathBuf::from(WEB_DIST_EMBED_ANCHOR),
            PathBuf::from("src/lib.rs"),
        ]),
        "removing web output must rebuild the embed consumer and union safe content changes"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn web_dist_structural_changes_require_a_regular_verified_anchor() {
    use std::os::unix::fs::symlink;

    let base = base();
    let root = base.join("source");
    let before = source_manifest(&[("web", b'D', b"", 0o755), ("web/dist", b'D', b"", 0o755)]);
    let after = source_manifest(&[
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/index.html", b'F', b"new", 0o644),
    ]);
    let removed_before = source_manifest(&[
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/assets", b'D', b"", 0o755),
        ("web/dist/assets/old.js", b'F', b"old asset", 0o644),
    ]);
    let removed_after = source_manifest(&[
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/assets", b'D', b"", 0o755),
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &after).action,
        SourceInvalidation::Workspace,
        "a missing anchor cannot specialize a generic workspace"
    );
    assert_eq!(
        source_invalidation_report(&root, Some(&removed_before), &removed_after).action,
        SourceInvalidation::Workspace,
        "a missing anchor cannot narrow a lone asset removal to its surviving parent"
    );

    install_test_web_embed_anchor(&root);
    let anchor = root.join(WEB_DIST_EMBED_ANCHOR);
    std::fs::write(&anchor, ACTUAL_WEB_EMBED_SOURCE.replace("../../web/dist", "../../web/other")).unwrap();
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &after).action,
        SourceInvalidation::Workspace,
        "an anchor for another folder cannot specialize web/dist changes"
    );
    assert_eq!(
        source_invalidation_report(&root, Some(&removed_before), &removed_after).action,
        SourceInvalidation::Workspace,
        "a wrong-folder anchor cannot narrow a lone asset removal to its surviving parent"
    );

    let target = base.join("embed-anchor-target.rs");
    std::fs::write(&target, TEST_WEB_EMBED_SOURCE).unwrap();
    std::fs::remove_file(&anchor).unwrap();
    symlink(&target, &anchor).unwrap();
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &after).action,
        SourceInvalidation::Workspace,
        "a symlink anchor is not trusted even when its target looks valid"
    );
    assert_eq!(
        source_invalidation_report(&root, Some(&removed_before), &removed_after).action,
        SourceInvalidation::Workspace,
        "a symlink anchor cannot narrow a lone asset removal to its surviving parent"
    );
    assert_eq!(
        source_invalidation_report(&root, None, &after).action,
        SourceInvalidation::Workspace,
        "a missing manifest baseline remains a full-workspace fallback"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn web_dist_fake_comment_and_raw_string_anchor_blocks_fall_back() {
    let base = base();
    let root = base.join("source");
    let before = source_manifest(&[("web", b'D', b"", 0o755)]);
    let after = source_manifest(&[
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/index.html", b'F', b"stub", 0o644),
    ]);
    let fake_declaration = WEB_DIST_EMBED_DECLARATION.join("\n");
    let anchor = root.join(WEB_DIST_EMBED_ANCHOR);
    std::fs::create_dir_all(anchor.parent().unwrap()).unwrap();

    std::fs::write(&anchor, format!("/*\n{fake_declaration}\n*/\n")).unwrap();
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &after).action,
        SourceInvalidation::Workspace,
        "a declaration-shaped block comment is not a RustEmbed consumer"
    );

    std::fs::write(&anchor, format!("const FAKE: &str = r#\"{fake_declaration}\"#;\n")).unwrap();
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &after).action,
        SourceInvalidation::Workspace,
        "a declaration-shaped raw string is not a RustEmbed consumer"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn web_dist_structure_mixed_with_nonweb_regular_file_removal_is_workspace() {
    let base = base();
    let root = base.join("source");
    install_test_web_embed_anchor(&root);
    let common = [
        (WEB_DIST_EMBED_ANCHOR, b'F', ACTUAL_WEB_EMBED_SOURCE.as_bytes(), 0o644),
        ("src", b'D', b"", 0o755),
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("web/dist/index.html", b'F', b"index", 0o644),
    ];
    let before = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("src/removed.rs", b'F', b"old", 0o644),
    ]);
    let removed_only = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &removed_only).action,
        SourceInvalidation::Files(vec![PathBuf::from("src")]),
        "ordinary non-web file deletion keeps its existing parent-touch behavior"
    );

    let web_added_and_removed = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/assets", b'D', b"", 0o755),
        ("web/dist/assets/new-hash.js", b'F', b"new", 0o644),
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &web_added_and_removed).action,
        SourceInvalidation::Workspace,
        "a surviving-parent non-web file deletion is not unioned with web structure changes"
    );

    let content_only = source_manifest(&[
        common[0], common[1], common[2], common[3],
        ("web/dist/index.html", b'F', b"updated index", 0o644),
        ("src/removed.rs", b'F', b"old", 0o644),
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &content_only).action,
        SourceInvalidation::Files(vec![PathBuf::from("web/dist/index.html")]),
        "content-only web changes do not count as embed structure changes"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn web_dist_changes_do_not_mask_other_structural_control_symlink_or_unsafe_changes() {
    use std::os::unix::fs::symlink;

    let base = base();
    let root = base.join("source");
    install_test_web_embed_anchor(&root);
    let common = [
        (WEB_DIST_EMBED_ANCHOR, b'F', ACTUAL_WEB_EMBED_SOURCE.as_bytes(), 0o644),
        ("web", b'D', b"", 0o755),
        ("web/dist", b'D', b"", 0o755),
        ("src", b'D', b"", 0o755),
        ("src/lib.rs", b'F', b"same", 0o644),
    ];
    let before = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/index.html", b'F', b"index", 0o644),
    ]);
    let web_add = ("web/dist/assets/new-hash.js", b'F', b"asset".as_slice(), 0o644);

    for (path, kind, value, mode) in [
        ("crates/other.txt", b'F', b"asset".as_slice(), 0o644),
        ("Cargo.lock", b'F', b"lock".as_slice(), 0o644),
        ("web/dist/../outside", b'F', b"unsafe".as_slice(), 0o644),
    ] {
        let mut entries = before.entries.iter().map(|(path, entry)| {
            (String::from_utf8_lossy(path).into_owned(), entry.kind, entry.value.as_slice(), entry.mode)
        }).collect::<Vec<_>>();
        entries.push(("web/dist/assets/new-hash.js".to_string(), web_add.1, web_add.2, web_add.3));
        entries.push((path.to_string(), kind, value, mode));
        let entries = entries.iter().map(|(path, kind, value, mode)| (path.as_str(), *kind, *value, *mode)).collect::<Vec<_>>();
        let current = source_manifest(&entries);
        assert_eq!(
            source_invalidation_report(&root, Some(&before), &current).action,
            SourceInvalidation::Workspace,
            "web/dist additions cannot hide non-web structural, Cargo control, or unsafe changes"
        );
    }

    let before_with_removed_inputs = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/index.html", b'F', b"index", 0o644),
        ("crates/other", b'D', b"", 0o755),
        ("Cargo.lock", b'F', b"lock", 0o644),
    ]);
    let web_only_added = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/index.html", b'F', b"index", 0o644),
        ("web/dist/assets", b'D', b"", 0o755),
        web_add,
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before_with_removed_inputs), &web_only_added).action,
        SourceInvalidation::Workspace,
        "web/dist changes cannot hide non-web structural or Cargo control removals"
    );

    let type_changed = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/index.html", b'D', b"", 0o755),
        web_add,
    ]);
    assert_eq!(
        source_invalidation_report(&root, Some(&before), &type_changed).action,
        SourceInvalidation::Workspace,
        "a web/dist type transition remains global"
    );

    std::fs::create_dir_all(root.join("web/dist/assets")).unwrap();
    std::fs::write(root.join("web/dist/assets/target.js"), b"target").unwrap();
    symlink("target.js", root.join("web/dist/assets/link.js")).unwrap();
    let linked = source_manifest(&[
        common[0], common[1], common[2], common[3], common[4],
        ("web/dist/index.html", b'F', b"index", 0o644),
        ("web/dist/assets", b'D', b"", 0o755),
        ("web/dist/assets/target.js", b'F', b"target", 0o644),
        ("web/dist/assets/link.js", b'L', b"target.js", 0o777),
    ]);
    let linked_added = source_invalidation_report(&root, Some(&before), &linked);
    assert_eq!(linked_added.action, SourceInvalidation::Workspace);
    let linked_removed = source_invalidation_report(&root, Some(&linked), &before);
    assert_eq!(linked_removed.action, SourceInvalidation::Workspace);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn directory_and_external_symlinks_force_workspace_invalidation() {
    use std::os::unix::fs::symlink;

    for target_is_directory in [false, true] {
        let base = base();
        let root = base.join("source");
        let target = if target_is_directory {
            base.join("outside-dir")
        } else {
            base.join("outside-file")
        };
        if target_is_directory {
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("input.txt"), "included\n").unwrap();
        } else {
            std::fs::write(&target, "included\n").unwrap();
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "old\n").unwrap();
        symlink(&target, root.join("include-link")).unwrap();
        let before = scan_source_manifest(&root, &[]).unwrap();
        std::fs::write(root.join("src/lib.rs"), "new\n").unwrap();
        let after = scan_source_manifest(&root, &[]).unwrap();
        assert_eq!(
            source_invalidation(&root, Some(&before), &after),
            SourceInvalidation::Workspace,
            "directory and workspace-external symlinks make changed-input scope unknown"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    let base = base();
    let root = base.join("source");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("assets/value.txt"), "old included input\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "old source\n").unwrap();
    symlink("assets", root.join("assets-link")).unwrap();
    let before = scan_source_manifest(&root, &[]).unwrap();
    std::fs::write(root.join("assets/value.txt"), "new included input\n").unwrap();
    let after = scan_source_manifest(&root, &[]).unwrap();
    assert_eq!(
        source_invalidation(&root, Some(&before), &after),
        SourceInvalidation::Workspace,
        "content changes below an unchanged in-workspace directory symlink must invalidate the full workspace"
    );
    let _ = std::fs::remove_dir_all(base);
}

fn fake_cargo_mtime_paths(dir: &Path) -> (PathBuf, PathBuf) {
    let fingerprint = dir.join("target/debug/.fingerprint/test-crate-hash/lib-test_crate.json");
    let dep_info = dir.join("target/debug/deps/test_crate-hash.d");
    std::fs::create_dir_all(fingerprint.parent().unwrap()).unwrap();
    std::fs::create_dir_all(dep_info.parent().unwrap()).unwrap();
    std::fs::write(&fingerprint, "fingerprint\n").unwrap();
    std::fs::write(&dep_info, "dep-info\n").unwrap();
    (fingerprint, dep_info)
}

#[test]
fn file_touch_moves_past_equal_and_future_cargo_mtimes() {
    for artifact_offset in [0, 3600] {
        let base = base();
        let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let dir = base.join("rc/shared");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let changed = dir.join("src/lib.rs");
        std::fs::write(&changed, "source\n").unwrap();
        let artifact_time = std::time::SystemTime::now()
            + Duration::from_secs(artifact_offset);
        set_mtime(&changed, artifact_time);
        let (fingerprint, dep_info) = fake_cargo_mtime_paths(&dir);
        set_mtime(&fingerprint, artifact_time);
        set_mtime(&dep_info, artifact_time);

        with_fake_transport(&base, || {
            touch_remote_files(
                &remote,
                &data,
                &dir.display().to_string(),
                &[PathBuf::from("src/lib.rs")],
                &Deadline::new(60),
            )
        })
        .expect("file invalidation should move beyond the Cargo cache baseline");

        if artifact_offset == 0 {
            let changed_mtime = std::fs::metadata(&changed).unwrap().modified().unwrap();
            assert!(changed_mtime > std::fs::metadata(&fingerprint).unwrap().modified().unwrap());
            assert!(changed_mtime > std::fs::metadata(&dep_info).unwrap().modified().unwrap());
        } else {
            assert!(!fingerprint.exists(), "future-dated Cargo fingerprints must be invalidated after a clock rollback");
            assert!(!dep_info.exists(), "future-dated dep-info must be invalidated after a clock rollback");
        }
        let _ = std::fs::remove_dir_all(base);
    }
}

#[test]
fn workspace_touch_moves_past_equal_and_future_cargo_mtimes() {
    for artifact_offset in [0, 3600] {
        let base = base();
        let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let dir = base.join("rc/shared");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let source = dir.join("src/lib.rs");
        std::fs::write(&source, "source\n").unwrap();
        let artifact_time = std::time::SystemTime::now()
            + Duration::from_secs(artifact_offset);
        set_mtime(&source, artifact_time);
        let (fingerprint, dep_info) = fake_cargo_mtime_paths(&dir);
        set_mtime(&fingerprint, artifact_time);
        set_mtime(&dep_info, artifact_time);

        with_fake_transport(&base, || {
            touch_remote_workspace(
                &remote,
                &data,
                &dir.display().to_string(),
                &Deadline::new(60),
            )
        })
        .expect("workspace invalidation should move beyond the Cargo cache baseline");

        if artifact_offset == 0 {
            let source_mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
            assert!(source_mtime > std::fs::metadata(&fingerprint).unwrap().modified().unwrap());
            assert!(source_mtime > std::fs::metadata(&dep_info).unwrap().modified().unwrap());
        } else {
            assert!(!fingerprint.exists(), "future-dated Cargo fingerprints must be invalidated after a clock rollback");
            assert!(!dep_info.exists(), "future-dated dep-info must be invalidated after a clock rollback");
        }
        let _ = std::fs::remove_dir_all(base);
    }
}

#[test]
fn cargo_target_symlinks_fail_closed_for_file_and_workspace_invalidation() {
    for (link_path, external_name, external_artifact, visible_trigger) in [
        (
            "target/debug",
            "external-debug",
            ".fingerprint/test-crate-hash/lib-test_crate.json",
            "target/release/deps/trigger.d",
        ),
        (
            "target/debug/.fingerprint",
            "external-fingerprint",
            "test-crate-hash/lib-test_crate.json",
            "target/debug/deps/test-crate-hash.d",
        ),
        (
            "target/debug/deps",
            "external-deps",
            "test_crate-hash.d",
            "target/debug/.fingerprint/test-crate-hash/lib-test_crate.json",
        ),
    ] {
        let base = base();
        let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let dir = base.join("rc/shared");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let changed = dir.join("src/lib.rs");
        std::fs::write(&changed, "source\n").unwrap();
        set_mtime(&changed, std::time::UNIX_EPOCH + Duration::from_secs(10));
        let _ = fake_cargo_mtime_paths(&dir);

        let link = dir.join(link_path);
        let external = base.join(external_name);
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::rename(&link, &external).unwrap();
        symlink(&external, &link).unwrap();

        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        let external_artifact = external.join(external_artifact);
        set_mtime(&external_artifact, future);
        let visible_trigger = dir.join(visible_trigger);
        std::fs::create_dir_all(visible_trigger.parent().unwrap()).unwrap();
        if !visible_trigger.exists() {
            std::fs::write(&visible_trigger, "visible cache trigger\n").unwrap();
        }
        set_mtime(&visible_trigger, future);
        let source_before = std::fs::metadata(&changed).unwrap().modified().unwrap();
        let external_artifact_before = std::fs::metadata(&external_artifact).unwrap().modified().unwrap();

        with_fake_transport(&base, || {
            touch_remote_files(
                &remote,
                &data,
                &dir.display().to_string(),
                &[PathBuf::from("src/lib.rs")],
                &Deadline::new(60),
            )
        })
        .expect("a symlinked Cargo cache path must discard the shared target before touching sources");
        assert!(
            !dir.join("target").exists(),
            "a symlink at {link_path} must invalidate the complete local target tree"
        );
        assert!(std::fs::metadata(&changed).unwrap().modified().unwrap() > source_before);
        assert!(external_artifact.exists(), "external cache artifacts must not be followed or removed");
        assert_eq!(std::fs::metadata(&external_artifact).unwrap().modified().unwrap(), external_artifact_before);
        let _ = std::fs::remove_dir_all(base);
    }

    let base = base();
    let (remote, _cwd, data) = fake_remote(&base, 60, "exit 0\n");
    let dir = base.join("rc/shared");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let source = dir.join("src/lib.rs");
    std::fs::write(&source, "source\n").unwrap();
    set_mtime(&source, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let (_, dep_info) = fake_cargo_mtime_paths(&dir);
    let fingerprint_dir = dir.join("target/debug/.fingerprint");
    let external = base.join("external-workspace-fingerprint");
    std::fs::rename(&fingerprint_dir, &external).unwrap();
    symlink(&external, &fingerprint_dir).unwrap();
    let future = std::time::SystemTime::now() + Duration::from_secs(3600);
    set_mtime(&external.join("test-crate-hash/lib-test_crate.json"), future);
    set_mtime(&dep_info, future);
    let source_before = std::fs::metadata(&source).unwrap().modified().unwrap();
    let fingerprint_before = std::fs::metadata(&external.join("test-crate-hash/lib-test_crate.json"))
        .unwrap()
        .modified()
        .unwrap();

    with_fake_transport(&base, || {
        touch_remote_workspace(
            &remote,
            &data,
            &dir.display().to_string(),
            &Deadline::new(60),
        )
    })
    .expect("workspace invalidation must discard a target tree with nested cache symlinks");
    assert!(!dir.join("target").exists());
    assert!(std::fs::metadata(&source).unwrap().modified().unwrap() > source_before);
    assert_eq!(
        std::fs::metadata(&external.join("test-crate-hash/lib-test_crate.json"))
            .unwrap()
            .modified()
            .unwrap(),
        fingerprint_before
    );
    assert!(!dep_info.exists(), "discarding the target must remove its local dep-info");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn sync_touches_only_a_changed_regular_file() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(cwd.join("src/lib.rs"), "old\n").unwrap();
    std::fs::write(cwd.join("unrelated.txt"), "same\n").unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("initial sync should establish a successful manifest");

    let unrelated = Path::new(&dir).join("unrelated.txt");
    set_mtime(&unrelated, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let unrelated_mtime = std::fs::metadata(&unrelated).unwrap().modified().unwrap();
    let changed = Path::new(&dir).join("src/lib.rs");
    std::fs::write(cwd.join("src/lib.rs"), "new\n").unwrap();
    sync().expect("one regular source-file change should synchronize");

    assert_eq!(
        std::fs::metadata(&unrelated).unwrap().modified().unwrap(),
        unrelated_mtime,
        "unrelated source files must keep their mtimes"
    );
    assert!(
        std::fs::metadata(&changed).unwrap().modified().unwrap() > unrelated_mtime,
        "the changed source file must be newer than the shared target baseline"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn unchanged_sync_refuses_lease_and_target_symlinks_before_rsync() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(cwd.join("src/lib.rs"), "old source\n").unwrap();
    let dir = base.join("rc").join(cache_hash(&cwd)).join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let sync = || with_fake_transport(&base, || sync_source(
        &remote, &data, &cwd, &dir.display().to_string(), &Deadline::new(60),
    ));
    sync().unwrap();
    let outside = base.join("outside-cache");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "must survive\n").unwrap();
    symlink(&outside, dir.join("target")).unwrap();
    assert!(sync().is_err(), "even unchanged inputs must refuse a target symlink");
    std::fs::remove_file(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    symlink(&outside, dir.join("target/debug")).unwrap();
    sync().expect("an unchanged run may safely discard a cache containing symlinks");
    assert!(!dir.join("target").exists());
    assert_eq!(std::fs::read_to_string(outside.join("keep")).unwrap(), "must survive\n");

    let outside_lease = base.join("outside-lease");
    std::fs::rename(&dir, &outside_lease).unwrap();
    symlink(&outside_lease, &dir).unwrap();
    std::fs::write(cwd.join("src/lib.rs"), "new source\n").unwrap();
    assert!(sync().is_err(), "a lease symlink must fail before rsync can follow it");
    assert_eq!(std::fs::read_to_string(outside_lease.join("src/lib.rs")).unwrap(), "old source\n");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn corrupt_source_manifest_falls_back_to_workspace_touch() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(cwd.join("unrelated.txt"), "same\n").unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("initial sync should establish a successful manifest");

    let unrelated = Path::new(&dir).join("unrelated.txt");
    set_mtime(&unrelated, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let before = std::fs::metadata(&unrelated).unwrap().modified().unwrap();
    std::fs::write(remote_manifest_path(&dir), b"corrupt manifest\n").unwrap();
    sync().expect("a corrupt manifest should recover by syncing and full invalidation");
    assert!(
        std::fs::metadata(&unrelated).unwrap().modified().unwrap() > before,
        "a corrupt baseline must full-touch the remote workspace"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn changing_one_crate_rechecks_only_it_and_its_dependent_crate() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        cwd.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/base\", \"apps/app\", \"crates/unrelated\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(cwd.join("Cargo.lock"), "version = 4\n").unwrap();
    for (name, path) in [
        ("base", "crates/base"),
        ("app", "apps/app"),
        ("unrelated", "crates/unrelated"),
    ] {
        std::fs::create_dir_all(cwd.join(path).join("src")).unwrap();
        if name == "base" {
            std::fs::create_dir_all(cwd.join(path).join("src/assets")).unwrap();
            std::fs::write(cwd.join(path).join("src/assets/one.txt"), "one\n").unwrap();
            std::fs::write(
                cwd.join(path).join("build.rs"),
                "use std::{env, fs, path::Path};\nfn main() { let assets = Path::new(\"src/assets\"); println!(\"cargo:rerun-if-changed={}\", assets.display()); let count = fs::read_dir(assets).unwrap().count(); fs::write(Path::new(&env::var(\"OUT_DIR\").unwrap()).join(\"assets.rs\"), format!(\"pub const ASSET_COUNT: u32 = {count};\\n\")).unwrap(); }\n",
            )
            .unwrap();
        }
        let dependencies = if name == "app" {
            "[dependencies]\nbase = { path = \"../../crates/base\" }\n"
        } else {
            ""
        };
        let build = if name == "base" { "build = \"build.rs\"\n" } else { "" };
        std::fs::write(
            cwd.join(path).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{build}{dependencies}"),
        )
        .unwrap();
        let source = if name == "base" {
            "include!(concat!(env!(\"OUT_DIR\"), \"/assets.rs\"));\npub fn value() -> u32 { ASSET_COUNT + 1 }\n"
        } else if name == "app" {
            "pub fn value() -> u32 { base::value() }\n"
        } else if name == "unrelated" {
            std::fs::create_dir_all(cwd.join(path).join("src/foo")).unwrap();
            std::fs::write(cwd.join(path).join("src/foo/mod.rs"), "pub fn nested() {}\n").unwrap();
            "mod foo;\npub fn value() -> u32 { 9 }\n"
        } else {
            "pub fn value() -> u32 { 9 }\n"
        };
        std::fs::write(cwd.join(path).join("src/lib.rs"), source).unwrap();
    }

    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let cargo = TestToolchain::current();
    let cargo_home = base.join("cargo-home");
    std::fs::create_dir_all(&cargo_home).unwrap();
    let lock = cargo
        .command()
        .args(["generate-lockfile", "--offline"])
        .current_dir(&cwd)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .unwrap();
    assert!(
        lock.status.success(),
        "cargo generate-lockfile failed: {}",
        String::from_utf8_lossy(&lock.stderr)
    );
    use_real_rsync_for_fake_remote(&base);
    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("initial workspace sync should succeed");
    let check = || {
        let output = cargo
            .command()
            .args(["check", "--offline", "--workspace", "--target-dir"])
            .arg(Path::new(&dir).join("target"))
            .current_dir(&dir)
            .env("CARGO_HOME", &cargo_home)
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap();
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "cargo check failed: {output_text}");
        output_text
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                line.strip_prefix("Checking ")
                    .or_else(|| line.strip_prefix("Compiling "))
            })
            .filter_map(|line| line.split_whitespace().next())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        check().len(),
        3,
        "the cold check should build all workspace packages"
    );

    std::fs::write(
        cwd.join("crates/base/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/assets.rs\"));\npub fn value() -> u32 { ASSET_COUNT + 2 }\n",
    )
    .unwrap();
    sync().expect("one crate source-file change should sync");
    let mut checked = check();
    checked.sort();
    assert_eq!(
        checked,
        vec!["app".to_owned(), "base".to_owned()],
        "only the changed crate and its dependent should be checked"
    );

    std::fs::write(cwd.join("crates/base/src/assets/two.txt"), "two\n").unwrap();
    sync().expect("adding a regular build input must invalidate the workspace");
    let mut checked = check();
    checked.sort();
    assert_eq!(
        checked,
        vec!["app".to_owned(), "base".to_owned(), "unrelated".to_owned()],
        "an added file must rebuild possible consumers absent from old dep-info"
    );

    std::fs::remove_file(cwd.join("crates/base/src/assets/one.txt")).unwrap();
    sync().expect("deleting a regular build input should sync narrowly");
    let mut checked = check();
    checked.sort();
    assert_eq!(
        checked,
        vec!["app".to_owned(), "base".to_owned()],
        "the surviving parent-directory touch must rerun its build-script consumer"
    );

    std::fs::write(cwd.join("crates/unrelated/src/foo.rs"), "pub fn sibling() {}\n").unwrap();
    sync().expect("adding a Rust module candidate should synchronize with full invalidation");
    let ambiguous = cargo
        .command()
        .args(["check", "--offline", "--workspace", "--target-dir"])
        .arg(Path::new(&dir).join("target"))
        .current_dir(&dir)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .unwrap();
    let ambiguity_output = format!(
        "{}{}",
        String::from_utf8_lossy(&ambiguous.stdout),
        String::from_utf8_lossy(&ambiguous.stderr)
    );
    assert!(!ambiguous.status.success(), "expected E0761 after adding an ambiguous module: {ambiguity_output}");
    assert!(ambiguity_output.contains("E0761"), "expected Rust module ambiguity E0761: {ambiguity_output}");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn run_keeps_project_cache_overrides_inside_the_lease() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let cargo = TestToolchain::current();
    let (remote, cwd, data) = fake_remote(&base, 60, &format!(
        "RUSTC={} RUSTDOC={} CARGO_HOME={} exec {} \"$@\"\n",
        sh_quote(&cargo.rustc.display().to_string()),
        sh_quote(&cargo.rustdoc.display().to_string()),
        sh_quote(&base.join("cargo-home").display().to_string()),
        sh_quote(&cargo.cargo.display().to_string()),
    ));
    std::fs::write(cwd.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
    let outside_target = base.join("outside-target");
    let outside_target_alias = base.join("outside-target-alias");
    let outside_build = base.join("outside-build");
    // Model variables inherited by the SSH shell; the helper must remove them before Cargo
    // starts (some Cargo initialization happens before command-line config is applied).
    write_exec(&base.join("fakebin/ssh"), &format!(
        "for a; do last=$a; done\nHOME={} CARGO_TARGET_DIR={} CARGO_BUILD_TARGET_DIR={} CARGO_BUILD_BUILD_DIR={} exec setsid sh -c \"$last\"\n",
        sh_quote(&base.join("home").display().to_string()),
        sh_quote(&outside_target.display().to_string()),
        sh_quote(&outside_target_alias.display().to_string()),
        sh_quote(&outside_build.display().to_string()),
    ));
    std::fs::write(cwd.join(".cargo/config.toml"), format!(
        "[build]\ntarget-dir = {:?}\nbuild-dir = {:?}\n", outside_target, outside_build,
    )).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let rc = with_fake_transport(&base, || run_offload(
        &remote, &data, &cwd, &["check".into(), "--offline".into()], &Deadline::new(60),
    ));
    assert_eq!(rc, 0, "Cargo should run with its cache inside the shared lease");
    let shared = base.join("rc").join(cache_hash(&cwd)).join("shared");
    assert!(shared.join("target/debug/.fingerprint").is_dir());
    assert!(!outside_target.exists(), "project target-dir must not evade invalidation: {}",
        String::from_utf8_lossy(&Command::new("find").arg(&outside_target).args(["-maxdepth", "5"]).output().unwrap().stdout));
    assert!(!outside_build.exists(), "project build-dir must not evade invalidation");
    assert!(!outside_target_alias.exists(), "inherited CARGO_BUILD_TARGET_DIR must not escape the lease");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn added_non_rust_input_rebuilds_a_compile_time_directory_listing() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(cwd.join("Cargo.toml"), r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
[dependencies]
listing = { path = "listing" }
[workspace]
members = ["listing"]
"#).unwrap();
    std::fs::create_dir_all(cwd.join("listing/src")).unwrap();
    std::fs::write(cwd.join("listing/Cargo.toml"), r#"[package]
name = "listing"
version = "0.1.0"
edition = "2021"
[lib]
proc-macro = true
"#).unwrap();
    // Like asset embedding macros, this tracks every existing input through include_bytes!,
    // but a newly discovered input is absent from the previous compiler dep-info.
    std::fs::write(cwd.join("listing/src/lib.rs"), r#"extern crate proc_macro;
#[proc_macro]
pub fn count(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut inputs = std::fs::read_dir(root.join("assets")).unwrap()
        .map(|e| format!("include_bytes!({:?}).len()", e.unwrap().path()))
        .collect::<Vec<_>>();
    inputs.sort();
    format!("[{}].len()", inputs.join(",")).parse().unwrap()
}
"#).unwrap();
    std::fs::create_dir_all(cwd.join("assets")).unwrap();
    std::fs::write(cwd.join("assets/one.txt"), "one").unwrap();
    std::fs::write(cwd.join("src/lib.rs"), r#"pub const COUNT: usize = listing::count!();
#[test]
fn listing_matches_current_tree() {
    assert_eq!(COUNT, std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/assets")).unwrap().count());
}
"#).unwrap();
    let dir = base.join("rc").join(cache_hash(&cwd)).join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let run = || {
        with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir.display().to_string(), &Deadline::new(60)))
            .expect("source sync should succeed");
        let cargo = TestToolchain::current();
        let output = cargo
            .command()
            .args(["test", "--offline", "-p", "app", "--target-dir"])
            .arg(dir.join("target"))
            .args(["--", "listing_matches_current_tree"])
            .current_dir(&dir)
            .env("CARGO_HOME", base.join("cargo-home"))
            .output().unwrap();
        assert!(output.status.success(), "the embedded listing must match the synchronized tree: {}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    };
    run();
    std::fs::write(cwd.join("assets/two.txt"), "two").unwrap();
    run();
    std::fs::remove_file(cwd.join("assets/one.txt")).unwrap();
    run();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn web_dist_structural_sync_rebuilds_embed_contents_without_unrelated_build_script() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        cwd.join("Cargo.toml"),
        r#"[workspace]
members = ["crates/am-base", "apps/app", "crates/unrelated"]
resolver = "2"

[profile.test]
debug-assertions = false
"#,
    )
    .unwrap();
    std::fs::create_dir_all(cwd.join("crates/am-base/src")).unwrap();
    std::fs::write(cwd.join("crates/am-base/Cargo.toml"), r#"[package]
name = "am-base"
version = "0.1.0"
edition = "2021"

[features]
default = ["embed-ui"]
embed-ui = []

[dependencies]
rust-embed = "8"
"#).unwrap();
    std::fs::write(cwd.join(WEB_DIST_EMBED_ANCHOR), TEST_WEB_EMBED_SOURCE).unwrap();
    std::fs::write(cwd.join("crates/am-base/src/lib.rs"), "pub mod assets;\n").unwrap();

    std::fs::create_dir_all(cwd.join("apps/app/src")).unwrap();
    std::fs::write(cwd.join("apps/app/Cargo.toml"), r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
am-base = { path = "../../crates/am-base" }
"#).unwrap();
    std::fs::write(cwd.join("apps/app/src/lib.rs"), r#"#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    fn read_tree(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        if !dir.exists() {
            return;
        }
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                read_tree(root, &path, files);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_string_lossy();
                files.insert(relative.into_owned(), std::fs::read(path).unwrap());
            }
        }
    }

    #[test]
    fn embedded_inventory_and_bytes_match_the_current_dist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
        let mut expected = BTreeMap::new();
        read_tree(&root, &root, &mut expected);
        assert!(am_base::assets::files() == expected, "embedded files differ from web/dist");
    }
}
"#).unwrap();

    std::fs::create_dir_all(cwd.join("crates/unrelated/src")).unwrap();
    std::fs::write(cwd.join("crates/unrelated/Cargo.toml"), r#"[package]
name = "unrelated"
version = "0.1.0"
edition = "2021"
build = "build.rs"
"#).unwrap();
    std::fs::write(cwd.join("crates/unrelated/build.rs"), r#"use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let counter = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("runs");
    let runs = fs::read_to_string(&counter).ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0) + 1;
    fs::write(counter, runs.to_string()).unwrap();
}
"#).unwrap();
    std::fs::write(cwd.join("crates/unrelated/src/lib.rs"), "pub fn stable() {}\n").unwrap();

    let cargo = TestToolchain::current();
    let lock = cargo
        .command()
        .args(["generate-lockfile", "--offline"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(lock.status.success(), "fake workspace lockfile generation failed");

    let dir = base.join("rc").join(cache_hash(&cwd)).join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);
    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir.display().to_string(), &Deadline::new(60))
        })
        .expect("fake workspace source sync should succeed");
    };
    let test = || {
        let output = cargo
            .command()
            .args(["test", "--offline", "--workspace", "--target-dir"])
            .arg(dir.join("target"))
            .current_dir(&dir)
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap();
        assert!(output.status.success(), "isolated embed fixture test failed");
    };
    let unrelated_build_runs = || {
        std::fs::read_dir(dir.join("target/debug/build"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("unrelated-"))
            .find_map(|entry| std::fs::read_to_string(entry.path().join("out/runs")).ok())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("unrelated crate build counter should exist after the initial test")
    };

    // Keep the existing web parent in the baseline; only web/dist is added below.
    std::fs::create_dir_all(cwd.join("web")).unwrap();
    sync();
    test();
    let initial_counter = unrelated_build_runs();

    std::fs::create_dir_all(cwd.join("web/dist/assets")).unwrap();
    std::fs::write(cwd.join("web/dist/index.html"), b"index-v1").unwrap();
    std::fs::write(cwd.join("web/dist/assets/app-a1b2.js"), b"asset-v1").unwrap();
    sync();
    test();
    assert_eq!(
        unrelated_build_runs(),
        initial_counter,
        "targeted embed invalidation must not rerun an unrelated crate build script"
    );

    std::fs::remove_dir_all(cwd.join("web/dist")).unwrap();
    sync();
    test();
    assert_eq!(
        unrelated_build_runs(),
        initial_counter,
        "removing embedded output must not rerun an unrelated crate build script"
    );
    let _ = std::fs::remove_dir_all(base);
}

fn set_mtime(path: &Path, modified: std::time::SystemTime) {
    std::fs::File::options().write(true).open(path).unwrap().set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
}

fn add_linked_worktree(a: &Path, b: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(a).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "cache-test@example.invalid"]);
    git(&["config", "user.name", "cache test"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "initial"]);
    git(&["worktree", "add", "-q", "-b", "cache-test-b", b.to_str().unwrap()]);
}

/// 同 repo 的第二個同時呼叫無法拿 shared.lock 時用獨立 job target；任何時刻都沒有兩個 writer 指向同一 target。
#[test]
fn concurrent_worktrees_never_write_the_same_shared_target() {
    let base = base();
    let a = base.join("repo-a");
    std::fs::create_dir_all(a.join("src")).unwrap();
    std::fs::write(a.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(a.join("src/value"), "A").unwrap();
    let b = base.join("repo-b");
    add_linked_worktree(&a, &b);
    let hash_a = cache_hash(&a);
    let hash_b = cache_hash(&b);
    assert_eq!(hash_a, hash_b, "both callers must contend for the same repo lease");

    let mut first = guard_with(&base, &hash_a, "job-1-1", QUICK);
    let mut second = guard_with(&base, &hash_b, "job-2-2", QUICK);
    assert_eq!(first.hs.dir, format!("{}/rc/{hash_a}/shared", base.display()));
    assert_eq!(second.hs.dir, format!("{}/rc/{hash_b}/job-2-2", base.display()));
    let a_target = PathBuf::from(&first.hs.dir).join("target");
    let b_target = PathBuf::from(&second.hs.dir).join("target");
    assert_ne!(a_target, b_target, "concurrent Cargo writers must never share target");
    std::fs::create_dir_all(&a_target).unwrap();
    std::fs::create_dir_all(&b_target).unwrap();
    std::fs::write(a_target.join("marker"), "A").unwrap();
    std::fs::write(b_target.join("marker"), "B").unwrap();
    second.finish();
    assert_eq!(std::fs::read(a_target.join("marker")).unwrap(), b"A", "the second writer must not corrupt shared artifacts");
    assert!(!b_target.exists(), "the isolated job target is removed after its lease");
    first.finish();
    assert_eq!(std::fs::read(a_target.join("marker")).unwrap(), b"A", "the shared cache remains intact");
    let _ = std::fs::remove_dir_all(base);
}

/// 兩棵 linked worktree 輪流使用同一個 target。B 的來源內容不同且 mtime 比舊產物早；rsync 必須更新遠端來源 mtime，
/// 才能讓 Cargo 的時間戳 freshness check 重編，而不是沿用 A 的產物。
#[test]
fn worktrees_share_cache_and_older_changed_sources_rebuild() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, a, data) = fake_remote(
        &base,
        60,
        "mkdir -p target\nif [ ! -f target/fingerprint ] || [ src/value -nt target/fingerprint ]; then\n  echo build >> \"$HOME/build-count\"\n  cp src/value target/built-from\n  touch target/fingerprint\nfi\nexit 0\n",
    );
    std::fs::write(a.join("src/value"), "tree A\n").unwrap();
    let b = base.join("worktree-b");
    add_linked_worktree(&a, &b);
    std::fs::write(b.join("src/value"), "tree B\n").unwrap();
    set_mtime(&b.join("src/value"), std::time::UNIX_EPOCH + Duration::from_secs(10));
    assert_eq!(cache_hash(&a), cache_hash(&b), "linked worktrees must share the repo cache key");

    use_real_rsync_for_fake_remote(&base);
    let rc_a = with_fake_transport(&base, || run_offload(&remote, &data, &a, &["check".into()], &Deadline::new(60)));
    assert_eq!(rc_a, 0, "worktree A should populate the shared target");
    let shared = base.join("rc").join(cache_hash(&a)).join("shared");
    let fingerprint = shared.join("target/fingerprint");
    assert!(fingerprint.is_file());
    set_mtime(&fingerprint, std::time::UNIX_EPOCH + Duration::from_secs(20));
    assert!(std::fs::metadata(b.join("src/value")).unwrap().modified().unwrap() < std::fs::metadata(&fingerprint).unwrap().modified().unwrap());

    let rc_b = with_fake_transport(&base, || run_offload(&remote, &data, &b, &["check".into()], &Deadline::new(60)));
    assert_eq!(rc_b, 0, "worktree B should use the same shared target");
    assert_eq!(std::fs::read(shared.join("src/value")).unwrap(), b"tree B\n");
    assert!(std::fs::metadata(shared.join("src/value")).unwrap().modified().unwrap() > std::time::UNIX_EPOCH + Duration::from_secs(20));
    assert_eq!(std::fs::read(shared.join("target/built-from")).unwrap(), b"tree B\n", "the stale target artifact must rebuild from B");
    assert_eq!(std::fs::read_to_string(base.join("home/build-count")).unwrap().lines().count(), 2, "A and then B both rebuild");
    let _ = std::fs::remove_dir_all(base);
}

/// 權限變更會被 rsync 套用，但檔案內容不重傳；同步後必須推進遠端 mtime，Cargo 才會重跑監看的 build script。
#[test]
fn chmod_only_rsync_change_reruns_a_cargo_build_script() {
    if !has_program("rsync") {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;

    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        cwd.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(cwd.join("tools")).unwrap();
    let tool = cwd.join("tools/probe");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(
        cwd.join("build.rs"),
        r#"use std::{env, fs, os::unix::fs::PermissionsExt, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tool = root.join("tools/probe");
    println!("cargo:rerun-if-changed={}", tool.display());
    let executable = fs::metadata(&tool).unwrap().permissions().mode() & 0o111 != 0;
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("mode.rs"), format!("pub const TOOL_EXECUTABLE: bool = {executable};")).unwrap();
    use std::io::Write as _;
    writeln!(fs::OpenOptions::new().create(true).append(true).open(out.join("runs")).unwrap(), "{executable}").unwrap();
}
"#,
    )
    .unwrap();
    std::fs::write(cwd.join("src/lib.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/mode.rs\"));\n").unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);

    let sync = || with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60)));
    sync().expect("initial source sync should succeed");
    let target = Path::new(&dir).join("target");
    let run_cargo = || {
        let output = TestToolchain::current()
            .command()
            .args(["check", "--offline", "--quiet", "--target-dir"])
            .arg(&target)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "cargo check failed: {}", String::from_utf8_lossy(&output.stderr));
    };
    let runs_file = || {
        std::fs::read_dir(target.join("debug/build"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path().join("out/runs"))
            .find(|path| path.is_file())
            .expect("build script should write its run counter")
    };
    run_cargo();
    let runs = runs_file();
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().collect::<Vec<_>>(), ["false"]);
    let remote_tool = Path::new(&dir).join("tools/probe");
    // source 與 remote 的 mtime 都固定在過去：沒有真的 touch 時 remote 就還是這個時間（斷言會紅），
    // 有 touch 才會變成現在。不必等檔案系統的秒數精度，也不會因為 rsync 複製了「現在」的 source mtime 而假綠。
    set_mtime(&tool, std::time::UNIX_EPOCH + Duration::from_secs(10));
    set_mtime(&remote_tool, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let before = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
    let source_mtime = std::fs::metadata(&tool).unwrap().modified().unwrap();
    // Cargo compares rerun-if-changed inputs against build/<hash>/output, not just
    // .fingerprint or .d. A clock rollback may leave only that baseline in the future.
    let build_output = runs.parent().unwrap().parent().unwrap().join("output");
    assert!(build_output.is_file());
    set_mtime(&build_output, std::time::SystemTime::now() + Duration::from_secs(3600));
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(std::fs::metadata(&tool).unwrap().modified().unwrap(), source_mtime, "chmod must not change the source mtime");
    sync().expect("permission-only source sync should succeed");
    let after = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
    assert!(after > before, "metadata-only rsync changes need a newer remote mtime");
    run_cargo();

    // Simulate rsync applying chmod before returning 23/24. The failed attempt must not
    // replace the last successful manifest; the next successful call still touches the file.
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
    let baseline_manifest = with_fake_transport(&base, || {
        read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
    })
    .unwrap()
    .unwrap()
    .encode();
    std::fs::write(base.join(".rsync-fail-after-copy"), "3\n").unwrap();
    assert!(sync().is_err(), "three rsync 24 exits should fail the sync");
    let after_failed_manifest = with_fake_transport(&base, || {
        read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
    })
    .unwrap()
    .unwrap()
    .encode();
    assert_eq!(
        after_failed_manifest, baseline_manifest,
        "failed sync must preserve last successful manifest"
    );
    let failed_mtime = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
    assert_eq!(
        failed_mtime, after,
        "chmod applied by failed retries must not be treated as invalidated"
    );
    // 恢復同步的 touch 要能看得出來：兩邊先固定在過去（同上，不靠 sleep 跨秒數精度）。
    set_mtime(&tool, std::time::UNIX_EPOCH + Duration::from_secs(20));
    set_mtime(&remote_tool, std::time::UNIX_EPOCH + Duration::from_secs(20));
    sync().expect("the next successful sync must invalidate the earlier failed chmod");
    let recovered_mtime = std::fs::metadata(&remote_tool).unwrap().modified().unwrap();
    assert!(
        recovered_mtime > failed_mtime,
        "manifest difference from the last success must touch the source"
    );
    run_cargo();
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().collect::<Vec<_>>(), ["false", "true", "false"], "Cargo must rerun after permission-only changes even when an earlier rsync retry failed");
    let _ = std::fs::remove_dir_all(base);
}

/// 長度前綴的二進位 manifest 可完整表示含 newline 的檔名，檔名內容也不能偽造成另一筆變更。
#[test]
fn source_manifest_handles_newline_names_without_forging_touch_paths() {
    if !has_program("rsync") {
        return;
    }
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    let name = std::ffi::OsString::from_vec(b"line\n[900] .f...p..... victim".to_vec());
    let source = cwd.join(&name);
    std::fs::write(&source, "script\n").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dir = format!("{}/rc/{}/shared", base.display(), cache_hash(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    use_real_rsync_for_fake_remote(&base);

    let sync = || {
        with_fake_transport(&base, || {
            sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60))
        })
    };
    sync().expect("first source manifest should synchronize");
    let remote_source = Path::new(&dir).join(&name);
    let remote_forged_paths = [Path::new(&dir).join("line"), Path::new(&dir).join("victim")];
    // 兩邊固定在過去（同 chmod 測試）：只有真的 touch 才會讓 remote 的 mtime 變新，不靠 sleep 跨秒數精度。
    set_mtime(&source, std::time::UNIX_EPOCH + Duration::from_secs(10));
    set_mtime(&remote_source, std::time::UNIX_EPOCH + Duration::from_secs(10));
    let before = std::fs::metadata(&remote_source)
        .unwrap()
        .modified()
        .unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    sync().expect("newline path metadata change should synchronize");

    let after = std::fs::metadata(&remote_source)
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        after > before,
        "the exact newline-containing path must be invalidated"
    );
    for forged in &remote_forged_paths {
        assert!(!forged.exists(), "filename bytes must not forge a second path to touch: {forged:?}");
    }
    let manifest = with_fake_transport(&base, || {
        read_remote_manifest(&remote, &data, &dir, &Deadline::new(60))
    })
    .unwrap()
    .unwrap();
    use std::os::unix::ffi::OsStrExt as _;
    assert!(
        manifest.entries.contains_key(name.as_bytes()),
        "manifest must retain the complete filename bytes"
    );
    assert!(!manifest.entries.contains_key(b"victim".as_slice()), "the filename suffix must not become a second manifest path");
    let _ = std::fs::remove_dir_all(base);
}

struct TestToolchain {
    cargo: PathBuf,
    rustc: PathBuf,
    rustdoc: PathBuf,
}

impl TestToolchain {
    fn current() -> Self {
        // Cargo supplies its own executable path. Enumerating rustup installations can pick
        // an unrelated old release (CI also has 1.57), even when stable built these tests.
        let cargo = PathBuf::from(
            std::env::var_os("CARGO")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| env!("CARGO").into()),
        );
        let bin = cargo.parent().expect("Cargo must have a parent directory");
        let tool = |key, name| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| bin.join(name))
        };
        Self {
            rustc: tool("RUSTC", "rustc"),
            rustdoc: tool("RUSTDOC", "rustdoc"),
            cargo,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.cargo);
        // rustdoc loads proc-macro metadata too; leaving it on PATH can mix compiler versions.
        command.env("RUSTC", &self.rustc).env("RUSTDOC", &self.rustdoc);
        command
    }
}

/// 真 Cargo path dependency 的 A→B→A：同一個測試名稱每次讀取 rsync 過去的來源檔，並與編入 test artifact 的 crate 值比較。
/// 若沿用另一棵 worktree 的 crate artifact，當前來源檔與編入的回傳值不同，該次測試就會失敗。
#[test]
fn path_dependency_worktrees_rebuild_the_matching_real_cargo_artifacts() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, a, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(a.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/am-base\", \"app\"]\nresolver = \"2\"\n").unwrap();
    std::fs::create_dir_all(a.join("crates/am-base/src")).unwrap();
    std::fs::write(
        a.join("crates/am-base/Cargo.toml"),
        "[package]\nname = \"am-base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(a.join("crates/am-base/src/a.txt"), "B\n").unwrap();
    std::fs::write(a.join("crates/am-base/src/b.txt"), "B\n").unwrap();
    symlink("a.txt", a.join("crates/am-base/src/input.txt")).unwrap();
    std::fs::write(
        a.join("crates/am-base/src/lib.rs"),
        "pub fn marker() -> &'static str { include_str!(\"input.txt\").trim() }\n",
    )
    .unwrap();
    std::fs::create_dir_all(a.join("app/src")).unwrap();
    std::fs::write(a.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nam-base = { path = \"../crates/am-base\" }\n").unwrap();
    std::fs::write(a.join("app/src/lib.rs"), "#[cfg(test)] mod tests { #[test] fn source_matches_current_tree() { let source = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../crates/am-base/src/input.txt\")).unwrap(); assert_eq!(am_base::marker(), source.trim()); } }\n").unwrap();
    let b = base.join("worktree-b");
    add_linked_worktree(&a, &b);
    std::fs::write(a.join("crates/am-base/src/a.txt"), "A\n").unwrap();
    std::fs::remove_file(b.join("crates/am-base/src/input.txt")).unwrap();
    symlink("b.txt", b.join("crates/am-base/src/input.txt")).unwrap();
    let old_source_mtime = std::time::UNIX_EPOCH + Duration::from_secs(10);
    set_mtime(&a.join("crates/am-base/src/a.txt"), old_source_mtime);
    set_mtime(&b.join("crates/am-base/src/b.txt"), old_source_mtime);
    assert_eq!(cache_hash(&a), cache_hash(&b));
    let dir = base.join("rc").join(cache_hash(&a)).join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(base.join("cargo-home")).unwrap();
    let cargo = TestToolchain::current();
    let run = |tree: &Path| {
        let sync = with_fake_transport(&base, || sync_source(&remote, &data, tree, &dir.display().to_string(), &Deadline::new(60)));
        sync.expect("source tree sync should succeed");
        assert_eq!(
            std::fs::read_to_string(dir.join("crates/am-base/src/input.txt")).unwrap(),
            std::fs::read_to_string(tree.join("crates/am-base/src/input.txt")).unwrap(),
            "the shared source tree must match the current worktree before Cargo runs"
        );
        let output = cargo
            .command()
            .args(["test", "--offline", "-p", "app", "--target-dir"])
            .arg(dir.join("target"))
            .args(["--", "source_matches_current_tree"])
            .current_dir(&dir)
            .env("CARGO_HOME", base.join("cargo-home"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success() && stdout.contains("running 1 test") && stdout.contains("source_matches_current_tree"), "{tree:?}: {stdout}\n{stderr}");
    };
    use_real_rsync_for_fake_remote(&base);
    run(&a);
    run(&b);
    run(&a);
    let _ = std::fs::remove_dir_all(base);
}

/// Root-level directory symlinks can feed include_str! from another workspace package;
/// any manifest difference must invalidate the consumer crate's Cargo fingerprint.
#[test]
fn cross_package_directory_symlink_rebuilds_the_matching_real_cargo_artifacts() {
    if !has_program("rsync") {
        return;
    }
    let base = base();
    let (remote, a, data) = fake_remote(&base, 60, "exit 0\n");
    std::fs::write(
        a.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/am-base\", \"app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(a.join("crates/am-base/src")).unwrap();
    std::fs::write(
        a.join("crates/am-base/Cargo.toml"),
        "[package]\nname = \"am-base\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(a.join("crates/am-base/src/lib.rs"), "pub fn marker() -> &'static str { \"ok\" }\n").unwrap();
    std::fs::create_dir_all(a.join("app/src")).unwrap();
    std::fs::write(
        a.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nam-base = { path = \"../crates/am-base\" }\n",
    )
    .unwrap();
    std::fs::write(
        a.join("app/src/lib.rs"),
        "pub const INCLUDED: &str = include_str!(\"../../assets/value.txt\");\n#[cfg(test)] mod tests { #[test] fn source_matches_current_tree() { let source = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../assets/value.txt\")).unwrap(); assert_eq!(super::INCLUDED.trim(), source.trim()); assert_eq!(am_base::marker(), \"ok\"); } }\n",
    )
    .unwrap();
    std::fs::create_dir_all(a.join("assets-a")).unwrap();
    std::fs::create_dir_all(a.join("assets-b")).unwrap();
    std::fs::write(a.join("assets-a/value.txt"), "asset A\n").unwrap();
    std::fs::write(a.join("assets-b/value.txt"), "asset B\n").unwrap();
    symlink("assets-a", a.join("assets")).unwrap();

    let b = base.join("worktree-b");
    add_linked_worktree(&a, &b);
    std::fs::remove_file(b.join("assets")).unwrap();
    symlink("assets-b", b.join("assets")).unwrap();
    let old_mtime = std::time::UNIX_EPOCH + Duration::from_secs(10);
    set_mtime(&a.join("assets-a/value.txt"), old_mtime);
    set_mtime(&a.join("assets-b/value.txt"), old_mtime);
    set_mtime(&b.join("assets-a/value.txt"), old_mtime);
    set_mtime(&b.join("assets-b/value.txt"), old_mtime);

    assert_eq!(cache_hash(&a), cache_hash(&b), "linked worktrees must share the repo cache key");
    let dir = base.join("rc").join(cache_hash(&a)).join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(base.join("cargo-home")).unwrap();
    let cargo = TestToolchain::current();
    let run = |tree: &Path| {
        let sync = with_fake_transport(&base, || {
            sync_source(&remote, &data, tree, &dir.display().to_string(), &Deadline::new(60))
        });
        sync.expect("source tree sync should succeed");
        let current = std::fs::read_to_string(tree.join("assets/value.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("assets/value.txt")).unwrap(), current);
        let output = cargo
            .command()
            .args(["test", "--offline", "-p", "app", "--target-dir"])
            .arg(dir.join("target"))
            .args(["--", "source_matches_current_tree"])
            .current_dir(&dir)
            .env("CARGO_HOME", base.join("cargo-home"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success() && stdout.contains("running 1 test") && stdout.contains("source_matches_current_tree"), "{tree:?}: {stdout}\n{stderr}");
    };
    use_real_rsync_for_fake_remote(&base);
    run(&a);
    run(&b);
    run(&a);
    let _ = std::fs::remove_dir_all(base);
}

/// 假 cargo 留下的 `cargo-started-<pid>`（在 `<root>/<hash>/`）裡的 pid。
fn started_cargo_pids(base: &Path) -> Vec<i32> {
    let mut pids = Vec::new();
    for hash in std::fs::read_dir(base.join("rc")).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(hash.path()).into_iter().flatten().flatten() {
            if let Some(pid) = e.file_name().to_string_lossy().strip_prefix("cargo-started-").and_then(|p| p.parse().ok()) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// 連線正常、遠端的 cargo 卡住（睡 300 秒）：超過上限（這裡 3 秒）就中止——回 124、遠端那顆 cargo 整組收掉、鎖與目錄還回去；
/// 而且**不退回本機**（124 不是 shim 認的 125）。以前沒有上限：一直等到有人 Ctrl-C。
#[test]
fn a_remote_build_over_the_limit_is_aborted_and_its_remote_processes_are_reaped() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 3, "touch \"$PWD/../cargo-started-$$\"\nexec sleep 45\n");
    let t0 = Instant::now();
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    let took = t0.elapsed();
    assert_eq!(rc, 124, "超過上限要回 124（不是 125＝退回本機，也不是 126＝失敗）");
    assert!(took >= Duration::from_secs(3), "上限沒到不能中止：{took:?}");
    assert!(took < Duration::from_secs(60), "超過上限之後要很快收乾淨：{took:?}");
    let pids = started_cargo_pids(&base);
    assert_eq!(pids.len(), 1, "假 cargo 真的起來過：{pids:?}");
    // 非當前行程之直系 child 無法接收 SIGCHLD/waitpid，觀察外部行程消亡只能靠 kill(pid, 0) 牆鐘輪詢
    wait_until("the remote cargo to be reaped", || unsafe { libc::kill(pids[0], 0) } != 0);
    // 守門收尾做完才回來：這次的 owner 撤掉了（＝目錄已還回去，晚到的 run 不會再跑）。
    let owners: Vec<_> = std::fs::read_dir(base.join("rc")).unwrap().flatten().flat_map(|h| std::fs::read_dir(h.path()).unwrap().flatten()).filter(|e| e.file_name().to_string_lossy().ends_with(".owner")).collect();
    assert!(owners.is_empty(), "目錄要還回去：{owners:?}");
    let _ = std::fs::remove_dir_all(base);
}

/// issue #428：ssh 連不進去（認證被拒、連不上、host key 不合）時，遠端**什麼都還沒跑**——
/// 這時要退回本機（125，shim 才會去拿本機名額），而不是把呼叫端的編譯整個弄失敗（126）。
/// 訊息要講得出「連的是誰、ssh 怎麼了、這次拿什麼登入」，結論要記進 data-dir 給 `/api/build-slots`。
#[test]
fn a_remote_that_refuses_the_login_falls_back_to_the_local_build_and_says_why() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    // 真的 ssh 被拒時就是這樣：訊息進 stderr、退出碼 255，stdout 一個字都沒有。
    // 多寫一行前置訊息：`reason` 要帶的是**最後**那一行（ssh 的結論），而且這一段等得到 stderr 讀完
    // （issue #467：以前沒等，Linux 上量到空字串、`reason` 變成「什麼都沒說」）。
    write_exec(
        &base.join("fakebin/ssh"),
        "echo 'OpenSSH_9.0, LibreSSL 3.3.6' >&2\necho 'me@fake: Permission denied (publickey,password).' >&2\nexit 255\n",
    );
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, EXIT_RUN_LOCALLY, "連不進去＝遠端沒動手，退回本機（不是 126＝驗證失敗）");

    let h = crate::remote_health::read(&data, "me@fake:22").expect("結論要記下來");
    assert!(!h.reachable, "{h:?}");
    assert_eq!(h.target, "me@fake:22", "記的是連哪一台（設定換主機之後看得出來是舊的）");
    let why = h.reason.as_deref().unwrap_or_default();
    assert!(why.contains("Permission denied"), "要帶 ssh 自己說的那一句，不是只有「回 255」：{h:?}");
    assert!(!why.contains("LibreSSL"), "帶的是最後那一行（結論），不是開頭的版本行：{h:?}");

    // 這一段就是印在 stderr 的東西：第一行要自己講完「連不上誰、改在本機跑」。
    let note = unreachable_note(&remote, &auth_desc(&remote, &data), why);
    let first = note.lines().next().unwrap();
    assert!(first.contains("me@fake:22") && first.contains("改在本機跑"), "第一行就要看得懂：{first}");
    let _ = std::fs::remove_dir_all(&base);
}

/// #1853：遠端只有一個名額且被別的 worktree 佔滿，helper 必須讓 shim 排本機。
#[test]
fn a_full_remote_overflows_to_local_without_running_cargo() {
    let base = base();
    let (mut remote, cwd, data) = fake_remote(&base, 60, "exit 99\n");
    remote.max_concurrent = 1;
    let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
    let t0 = Instant::now();
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, EXIT_RUN_LOCALLY, "滿額不能回 75 或執行遠端 cargo");
    assert!(t0.elapsed() < Duration::from_secs(30), "應短暫等候後退回本機");
    assert!(crate::remote_health::read(&data, "me@fake:22").unwrap().reachable);
    let events = std::fs::read_to_string(data.join(am_base::remote_event::EVENT_FILE)).unwrap();
    let events: Vec<serde_json::Value> = events.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "local_fallback");
    assert_eq!(events[0]["host"], remote.host);
    assert!(events[0]["slot_wait_secs"].as_u64().is_some());
    holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 同一件事的另一面（issue #428）：連得上、是遠端的 cargo 自己失敗——那是**真的驗證失敗**，
/// 原樣帶回它的退出碼，不能偷偷改成「在本機再跑一次」；而且這次算連得上。
#[test]
fn a_reachable_remote_whose_cargo_fails_is_not_a_fallback() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 101\n");
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, 101, "cargo 自己的退出碼原樣帶回，不是 125");
    let h = crate::remote_health::read(&data, "me@fake:22").expect("結論要記下來");
    assert!(h.reachable && h.reason.is_none(), "連得上：{h:?}");
    let events = std::fs::read_to_string(data.join(am_base::remote_event::EVENT_FILE)).unwrap();
    let events: Vec<serde_json::Value> = events.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(events.len(), 1, "record the selected lease even when cargo fails");
    assert_eq!(events[0]["outcome"], "shared");
    assert_eq!(events[0]["host"], remote.host);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn event_write_failure_or_lock_contention_does_not_change_build_result() {
    use std::os::unix::io::AsRawFd;
    for busy in [false, true] {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        let lock = data.join(am_base::remote_event::LOCK_FILE);
        std::fs::create_dir_all(&data).unwrap();
        let holder = if busy {
            let file = std::fs::File::create(&lock).unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
            Some(file)
        } else {
            std::fs::create_dir(&lock).unwrap();
            None
        };
        let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".into()], &Deadline::new(remote.timeout_secs)));
        assert_eq!(rc, 0, "event writer must not change a successful build");
        assert!(!data.join(am_base::remote_event::EVENT_FILE).exists());
        drop(holder);
        let _ = std::fs::remove_dir_all(base);
    }
}

/// 假 rsync：每跑一次在 `<base>/rsync-calls` 記一筆；第 N 次（1 起算）照 `fails` 第 N 個字元決定：`v`＝印 rsync 真的會印的
/// ENOENT 那行、回 23；`p`＝權限錯、回 23；`x`＝回 12（協定錯）；超出長度＝照常複製。回傳 sync_source 的結果與呼叫次數。
fn sync_with_fake_rsync(fails: &str) -> (anyhow::Result<()>, usize) {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    let calls = base.join("rsync-calls");
    write_exec(
        &base.join("fakebin/rsync"),
        &format!(
            "echo x >> '{calls}'\nn=$(wc -l < '{calls}' | tr -d ' ')\nc=$(printf '%s' '{fails}' | cut -c\"$n\")\n\
                 case \"$c\" in\n\
                 v) echo \"rsync(95387): error: $PWD/web/dist/assets/index-Cy7N34fd.js: open (2) in $PWD: No such file or directory\" >&2; exit 23 ;;\n\
                 p) echo 'rsync: opendir \"/x\" failed: Permission denied (13)' >&2; exit 23 ;;\n\
                 x) exit 12 ;;\n\
                 esac\n\
                 for a; do src=$dest; dest=$a; done\nd=${{dest#*:}}\nmkdir -p \"$d\" && cp -a \"${{src}}.\" \"$d\"\n",
            calls = calls.display()
        ),
    );
    let dir = base.join("dest").display().to_string();
    let r = with_fake_transport(&base, || sync_source(&remote, &data, &cwd, &dir, &Deadline::new(60)));
    let n = std::fs::read_to_string(&calls).unwrap_or_default().lines().count();
    if r.is_ok() {
        assert!(base.join("dest/Cargo.toml").exists(), "成功就真的同步過去了");
    }
    let _ = std::fs::remove_dir_all(&base);
    (r, n)
}

/// am-lead 2026-09-25：別的 bot 在主樹 `bun run build`，rsync 列完清單後 `web/dist` 的舊 hash 檔不見 → exit 23，整個 cargo 判死。
/// 來源在同步中途消失要短暫重試；一直消失才失敗、而且要講是來源樹被改動；其他 rsync 錯誤不重試。
#[test]
fn rsync_retries_only_when_the_source_changes_under_it() {
    let (r, n) = sync_with_fake_rsync("v");
    assert!(r.is_ok(), "第一次撞到檔案消失、第二次成功＝成功：{r:?}");
    assert_eq!(n, 2);

    let (r, n) = sync_with_fake_rsync("vvvvvv");
    let msg = format!("{:#}", r.expect_err("一直消失要失敗"));
    assert_eq!(n, SYNC_ATTEMPTS as usize, "重試有上限");
    assert!(msg.contains("exit Some(23)") && msg.contains("來源樹在同步時一直被改動") && msg.contains("web/dist"), "{msg}");

    for fails in ["p", "x"] {
        let (r, n) = sync_with_fake_rsync(fails);
        let msg = format!("{:#}", r.expect_err("別的錯誤照舊失敗"));
        assert_eq!(n, 1, "{fails}：不是來源消失就不重試");
        assert!(!msg.contains("來源樹"), "{fails}：不能誤報成來源被改動：{msg}");
    }
    assert!(source_changed_during_sync(Some(24), ""), "24＝vanished，一律算");
    assert!(!source_changed_during_sync(None, "No such file or directory"), "被訊號砍掉不算");
}

/// 訊息本身（純組字串）：這台機器 2026-09-24 的實況就是「沒設 identity_file、資料目錄也沒有密碼檔」，
/// 於是 ssh 拿預設金鑰去連、被拒。看訊息的人要能直接知道少了什麼，而不是只看到一行 Permission denied。
#[test]
fn the_note_names_the_host_and_which_credentials_were_used() {
    let base = base();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut remote = BuildRemoteCfg { enabled: true, host: "box".into(), user: "me".into(), ..Default::default() };

    let note = unreachable_note(&remote, &auth_desc(&remote, &data), "ssh 回 255");
    assert!(note.contains("me@box:22") && note.contains("ssh 回 255"), "{note}");
    assert!(note.lines().next().unwrap().contains("改在本機跑"), "第一行就要講接下來會怎麼樣：{note}");
    assert!(note.contains("identity_file") && note.contains("密碼檔"), "兩個都沒有就要兩個都點名：{note}");

    remote.identity_file = "/keys/build".into();
    let note = unreachable_note(&remote, &auth_desc(&remote, &data), "ssh 回 255");
    assert!(note.contains("/keys/build"), "設了金鑰就講是哪一把（被拒的是它）：{note}");

    std::fs::write(data.join(PASSWORD_FILE), "pw").unwrap();
    assert!(auth_desc(&remote, &data).contains("密碼檔"), "有密碼檔就先講密碼檔");
    let _ = std::fs::remove_dir_all(&base);
}

/// 沒超過上限的照常回傳結果（成功回 0、cargo 失敗回它的退出碼），不會被誤判成逾時。
#[test]
fn a_remote_build_within_the_limit_returns_its_own_result() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 101\n");
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, 101, "cargo 自己的退出碼原樣帶回");
    let _ = std::fs::remove_dir_all(&base);
    let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["check".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, 0);
    let _ = std::fs::remove_dir_all(base);
}

/// `timeout_secs = 0`＝不設上限：跑得比「一般的上限」久也照常等到結束。
#[test]
fn a_zero_limit_means_no_limit() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 0, "sleep 3\nexit 0\n");
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    assert_eq!(rc, 0);
    let _ = std::fs::remove_dir_all(base);
}

// ── 本機 helper 被砍時遠端也要停（issue #201）──

/// 本機 helper 被 `kill -9`：kernel 收掉它所有的 fd，守門那條 ssh 的 stdin 關掉（**不是**我們自己做清理）——遠端還在跑的 cargo 整組要在
/// 幾秒內消失，shared 的鎖也要放掉（不然同一棵 worktree 的下一次編譯會卡在等鎖，只能連到遠端手動清）。
#[test]
fn a_dead_helper_leaves_no_running_remote_process_and_releases_the_shared_lock() {
    let base = base();
    let home = fake_home(&base);
    let mut lease = guard(&base, HASH, "job-1-1"); // 搶到 shared
    let dir = lease.hs.dir.clone();
    assert!(dir.ends_with("/shared"), "{dir}");
    let mut run = spawn_run(&base, &home, &dir, &lease.token);
    wait_until("fake cargo to start", || started(&dir));
    let lock = format!("{dir}.lock");
    let lock_free = || Command::new("flock").args(["-n", &lock, "true"]).status().map(|s| s.success()).unwrap_or(false);
    assert!(!lock_free(), "cargo 在跑：鎖被守門持有");

    drop(lease.stdin.take()); // helper 死了：只剩連線中斷
    let status = wait_run(&mut run);
    assert!(status.signal().is_some() || !status.success(), "遠端的 cargo 要被收掉：{status:?}");
    // 守門結束時作業系統會自動釋放所有 flock，等 lease.child 結束即可確認鎖已放掉
    let _ = lease.child.wait();
    assert!(lock_free(), "守門結束後 shared 鎖必須已釋放");
    let _ = std::fs::remove_dir_all(base);
}

/// 離開了 process group 的殘留（cargo 底下自己 `setsid` 的輔助行程）`reap` 追不到：守門收尾時，還把 cwd 放在這個目錄裡的一併收掉。
#[test]
fn a_process_that_left_the_process_group_is_still_reaped_when_the_lease_ends() {
    let base = base();
    let home = fake_home(&base);
    write_exec(
        &home.join(".cargo/bin/cargo"),
        "touch \"$PWD/../cargo-started-$$\"\nsetsid sleep 45 >/dev/null 2>&1 &\necho $! > \"$PWD/../escaped.pid\"\nexec sleep 45\n",
    );
    let mut holder = guard(&base, HASH, "job-1-1"); // 佔住 shared，讓下一個拿 job 目錄
    let mut lease = guard(&base, HASH, "job-2-2");
    let dir = lease.hs.dir.clone();
    let mut run = spawn_run(&base, &home, &dir, &lease.token);
    let escaped_pid = || std::fs::read_to_string(Path::new(&dir).parent().unwrap().join("escaped.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok());
    wait_until("the escaped helper to be recorded", || escaped_pid().is_some());
    let pid = escaped_pid().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "離開 process group 的輔助行程還活著");

    drop(lease.stdin.take());
    assert!(lease.finish().unwrap().success());
    // 非直系 child 無法接收 SIGCHLD，觀察 setsid 逃逸行程消亡只能靠 kill(pid, 0) 牆鐘輪詢
    wait_until("the escaped helper to be reaped", || unsafe { libc::kill(pid, 0) } != 0);
    let _ = run.wait();
    holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 收到終止訊號（Ctrl-C／TERM／HUP）：helper 不再直接被打死、留下還連著的 ssh 與遠端的 cargo——把本機的 ssh／rsync 收掉、等守門把遠端那組行程
/// 收乾淨，才用 `128 + 訊號` 結束。這裡用測試專用的 abort 旗標模擬「收到 SIGTERM」（真的訊號只有正式 helper 才看）。
#[test]
fn an_interrupted_helper_reaps_the_remote_group_before_it_exits() {
    let base = base();
    let (remote, cwd, data) = fake_remote(&base, 0, "touch \"$PWD/../cargo-started-$$\"\nexec sleep 45\n");
    let ctl = Deadline::new(0);
    let abort = ctl.abort.clone();
    let watcher = {
        let base = base.clone();
        std::thread::spawn(move || {
            wait_until("the remote cargo to start", || !started_cargo_pids(&base).is_empty());
            abort.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let t0 = Instant::now();
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &ctl));
    watcher.join().unwrap();
    assert_eq!(rc, 128 + libc::SIGTERM, "被訊號中止：128＋訊號");
    assert!(t0.elapsed() < Duration::from_secs(60), "{:?}", t0.elapsed());
    let pids = started_cargo_pids(&base);
    assert_eq!(pids.len(), 1, "{pids:?}");
    // 非直系 child 無法接收 SIGCHLD，觀察假 cargo 行程消亡只能靠 kill(pid, 0) 牆鐘輪詢
    wait_until("the remote cargo to be reaped", || unsafe { libc::kill(pids[0], 0) } != 0);
    let _ = std::fs::remove_dir_all(base);
}

// ── 遠端名額（issue #104）──

/// 不同 worktree 共用同一組遠端名額：上限 2 時第三棵要排隊，前面有人還回名額它才拿到（拿到的就是空出來的那個）。
#[test]
fn different_worktrees_share_one_remote_cap() {
    let base = base();
    let adm = Admission { max: 2, queue_wait_secs: 60, ..QUICK };
    let mut a = guard_with(&base, "00000000000000a1", "job-1-1", adm);
    let mut b = guard_with(&base, "00000000000000b2", "job-2-2", adm);
    let mut got = vec![a.hs.slot, b.hs.slot];
    got.sort();
    assert_eq!(got, vec![Some((1, 2)), Some((2, 2))]);

    // 同步點是排隊中的心跳 `Q`：`Q` 只在號碼牌已經 flock 之後才印，讀到它才算「進了佇列」
    let queue_wait = adm.queue_wait_secs;
    let c_token = LeaseToken::new();
    let mut waiter = guard_cmd(&base, "00000000000000c3", "job-3-3", &c_token, adm)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(waiter.stdout.take().unwrap());
    wait_for_guard_queued(&mut out, queue_wait);

    let freed = a.hs.slot;
    a.finish();

    let (hs, _) = read_handshake_observed(&mut out);
    let c_hs = hs.expect("a 還回名額之後 waiter 要拿到目錄");
    assert_eq!(c_hs.slot, freed);

    b.finish();
    finish_waiter(waiter, queue_wait);
    let _ = std::fs::remove_dir_all(base);
}

/// 端到端（真的 `run_offload`＋真的守門）：四棵 worktree 同時跑、上限 2，任何時刻最多兩個遠端 cargo，而且四個都跑完、結果原樣帶回。
/// 修正前沒有遠端上限：四個一起跑（2026-09-19 就是 9 個冷編譯同時跑把遠端壓垮）。
/// 正式的排隊等待只有 3 秒（#1853），機器一忙前兩個超過 3 秒，後兩個就合法地退回本機（125）——這條驗的是上限與排隊，
/// 所以每條執行緒各自把排隊等待設長（#1880：thread_local 不會跟著 spawn 走）。
#[test]
fn concurrent_calls_from_different_worktrees_never_exceed_the_remote_cap() {
    let base = base();
    let marks = base.join("running");
    std::fs::create_dir_all(&marks).unwrap();
    let body = format!(
        "touch '{m}/'$$\nls '{m}' | wc -l >> '{m}.log'\nsleep 2\nrm -f '{m}/'$$\nexit 0\n",
        m = marks.display()
    );
    let (mut remote, _, data) = fake_remote(&base, 60, &body);
    remote.max_concurrent = 2;
    let calls: Vec<_> = (0..4)
        .map(|i| {
            let cwd = base.join(format!("wt{i}"));
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::write(
                cwd.join("Cargo.toml"),
                format!("[package]\nname = \"wt{i}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            let (remote, data, base) = (remote.clone(), data.clone(), base.clone());
            std::thread::spawn(move || {
                TEST_QUEUE_WAIT_SECS.with(|w| w.set(Some(60)));
                with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)))
            })
        })
        .collect();
    let rcs: Vec<i32> = calls.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(rcs, vec![0; 4]);
    let seen: Vec<u32> = std::fs::read_to_string(base.join("running.log")).unwrap().lines().map(|l| l.trim().parse().unwrap()).collect();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert!(seen.iter().all(|n| *n <= 2), "同時在跑的遠端 cargo 超過上限 2：{seen:?}");
    assert!(seen.contains(&2), "兩個名額都該用上：{seen:?}");
    let _ = std::fs::remove_dir_all(base);
}

/// 排隊的時間不算進整體上限：上限 3 秒、前面的人佔名額 4 秒、自己的 cargo 跑 1 秒 → 照常成功（不是 124）。
/// #1971：shared 被佔著的時候，正式會等 900 秒；測試密封後只等 [`SEALED_SHARED_WAIT_SECS`] 秒就改用獨立目錄（名額滿則退回本機）。
/// 這條能在 30 秒內結束＝覆寫確實生效（沒生效就會等滿 900 秒）。
#[test]
fn a_shared_wait_is_capped_under_test() {
    let base = base();
    let (mut remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
    remote.max_concurrent = 1;
    let hash = cache_hash(&cwd);
    let mut holder = guard_with(&base, &hash, "job-1-1", Admission { max: 1, pool: 1, ..QUICK });
    let t0 = Instant::now();
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    let took = t0.elapsed();
    holder.finish();
    assert_eq!(rc, EXIT_RUN_LOCALLY, "shared 等完改獨立目錄，但唯一的名額還被佔著＝排隊逾時退回本機");
    assert!(took >= Duration::from_secs(SEALED_SHARED_WAIT_SECS - 1), "確實等了上限那麼久：{took:?}");
    assert!(took < Duration::from_secs(30), "shared 等待沒被密封：{took:?}");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn time_spent_queueing_does_not_count_toward_the_overall_limit() {
    let base = base();
    let (mut remote, cwd, data) = fake_remote(&base, 3, "sleep 1\nexit 0\n");
    remote.max_concurrent = 1;
    // #1853：名額只等 3 秒後會 overflow；改等同 worktree 的 shared 鎖，仍驗證排隊不算執行逾時。
    let hash = cache_hash(&cwd);
    let mut holder = guard_with(&base, &hash, "job-1-1", Admission { max: 1, ..QUICK });
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        holder.finish();
    });
    let t0 = Instant::now();
    let rc = with_fake_transport(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(remote.timeout_secs)));
    let took = t0.elapsed();
    release.join().unwrap();
    assert!(took >= Duration::from_secs(4), "真的有排隊：{took:?}");
    assert_eq!(rc, 0, "排隊 4 秒＋跑 1 秒，上限 3 秒只算後面那段");
    let _ = std::fs::remove_dir_all(base);
}

/// shared 被佔著時先等它用完（拿到同一個 shared、增量編譯），不是馬上改用冷編譯的 job 目錄；等超過上限才改用 job 目錄。
#[test]
fn a_busy_shared_target_is_waited_for_and_only_given_up_after_a_limit() {
    let base = base();
    let mut a = guard(&base, HASH, "job-1-1");
    let adir = a.hs.dir.clone();
    assert!(adir.ends_with("/shared"), "{adir}");

    // 一個正常守門在等 shared：同步點是排隊中的心跳 `Q`
    let shared_wait = 30;
    let adm_b = Admission { shared_wait_secs: shared_wait, ..QUICK };
    let b_token = LeaseToken::new();
    let mut waiter = guard_cmd(&base, HASH, "job-2-2", &b_token, adm_b)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(waiter.stdout.take().unwrap());
    wait_for_guard_queued(&mut out, shared_wait);

    a.finish();
    let (hs, _) = read_handshake_observed(&mut out);
    let b_hs = hs.expect("a 用完之後 waiter 要拿到 shared 目錄");
    assert_eq!(b_hs.dir, adir, "waiter 拿到同一個 shared，不是 job-2-2");

    // 佔著不放：等 2 秒還沒空出來就改用 job 目錄。
    let t0 = Instant::now();
    let mut c = guard_with(&base, HASH, "job-3-3", Admission { shared_wait_secs: 2, ..QUICK });
    assert!(c.hs.dir.ends_with(&format!("/{HASH}/job-3-3")), "{:?}", c.hs);
    // guard 腳本用 `date +%s`（整秒）算已等多久：上限 2 秒實際會在 1～2 秒之間放棄（負載高時更明顯），
    // 要擋的是「馬上改用冷編譯」，所以門檻是至少等過 1 秒，不是剛好 2 秒。
    assert!(t0.elapsed() >= Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_eq!(c.obs.outcome.as_deref(), Some("isolated"));
    assert!(c.obs.shared_wait_secs.is_some_and(|seconds| seconds >= 1));
    assert!(c.obs.slot_wait_secs.is_some());
    c.finish();
    finish_waiter(waiter, shared_wait);
    let _ = std::fs::remove_dir_all(base);
}

/// 名額一直全滿：排到上限就放棄——不交目錄、exit 75（[`QueueFull`]），自己佔的 shared 鎖也放掉。
///
/// 「至少等滿 `queue_wait_secs`」這條以前是碰運氣的（issue #486）：守門用 `date +%s` 算經過時間，
/// truncate 到整秒會多算最多 1 秒，`qwait=2` 時真的只等約 1 秒就放棄，整樹平行下就偶發紅在下面那行
/// （量到 1.04 秒，而守門自己的 log 還說「等了 2 秒」）。守門改成 `-le` 之後這條是確定的。
#[test]
fn a_queue_that_never_moves_gives_up_with_exit_75() {
    let base = base();
    let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
    let token = LeaseToken::new();
    let cmd = guard_cmd(&base, HASH, "job-2-2", &token, Admission { max: 1, queue_wait_secs: 2, ..QUICK });
    let t0 = Instant::now();
    let err = Lease::start(cmd, token, &Deadline::new(0)).err().expect("queue full");
    assert!(err.downcast_ref::<QueueFull>().is_some(), "{err:#}");
    assert!(
        t0.elapsed() >= Duration::from_secs(2) && t0.elapsed() < Duration::from_secs(60),
        "要等滿 queue_wait_secs 才放棄（issue #486：整秒時鐘會少等將近一秒），實際等了 {:?}",
        t0.elapsed()
    );
    assert!(lock_free(&base.join(format!("rc/{HASH}/shared.lock"))), "放棄時要放掉 shared 的鎖");
    holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 開一個會排隊的守門，等它打出第一聲心跳，然後像 helper 那條 ssh 斷掉時遠端 sshd 做的一樣：**只關它的 stdin**，
/// stdout 繼續有人讀（2026-09-19 用真的 sshd 量：本機 ssh 被砍之後守門的 stdout 照樣寫得進去，只有 stdin 會讀到 EOF）。
/// 它要在幾秒內自己結束。
fn hang_up_while_queued(base: &Path, hash: &str, adm: Admission) {
    let token = LeaseToken::new();
    let mut child = guard_cmd(base, hash, "job-2-2", &token, adm)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while line.trim_end() != "Q" {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "守門還沒排隊就結束了");
        assert!(!line.starts_with("ROOT"), "應該要排隊，不該拿到目錄");
    }
    drop(child.stdin.take());
    let drain = std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
    // 斷線後守門應在數秒內主動結束並放鎖；若發生 bug 未退出則以 10 秒牆鐘超時 kill 防止卡死
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("helper 斷線了，排隊中的守門還一直佔著位置（{adm:?}）");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = drain.join();
}

/// 排隊中 helper 斷線了（被砍、Ctrl-C、網路斷）：守門靠心跳發現，自己結束、放掉手上的鎖，不佔著位置空等到上限。
/// 兩種排隊都一樣：等遠端名額（這時它握著自己 worktree 的 shared 鎖）、等 shared。
#[test]
fn a_queued_guard_whose_helper_went_away_leaves_the_queue() {
    let base = base();
    let lock = base.join(format!("rc/{HASH}/shared.lock"));
    let mut slot_holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
    hang_up_while_queued(&base, HASH, Admission { max: 1, queue_wait_secs: 120, ..QUICK });
    assert!(lock_free(&lock), "結束時放掉 shared 的鎖");
    slot_holder.finish();

    let mut shared_holder = guard(&base, HASH, "job-1-1");
    hang_up_while_queued(&base, HASH, Admission { shared_wait_secs: 120, ..QUICK });
    shared_holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// #326：冷編譯的 job 排隊中被掛斷（或 queue full），`job-*.lock` 不能留在遠端沒人收。
#[test]
fn a_queued_job_that_gives_up_leaves_no_lock_file_behind() {
    let base = base();
    let mut holder = guard_with(&base, HASH, "job-1-1", Admission { max: 1, ..QUICK });
    hang_up_while_queued(&base, HASH, Admission { max: 1, queue_wait_secs: 120, ..QUICK });
    let lock = base.join(format!("rc/{HASH}/job-2-2.lock"));
    assert!(!lock.exists(), "排隊中放棄的 job 留下了 {}", lock.display());
    holder.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 守門腳本裡的自動上限跟文件寫的算式一致（這台就是遠端：用它自己的核數與 RAM 核對）。
#[test]
fn the_guards_automatic_cap_matches_the_documented_formula() {
    let base = base();
    let ncpu: u64 = String::from_utf8(Command::new("getconf").arg("_NPROCESSORS_ONLN").output().unwrap().stdout).unwrap().trim().parse().unwrap();
    let mem_kb: u64 = std::fs::read_to_string("/proc/meminfo")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    for jobs in [1, 3, 8] {
        let mut g = guard_with(&base, HASH, "job-1-1", Admission { max: 0, jobs, ..QUICK });
        let want = super::tests::auto_max_concurrent(ncpu, mem_kb, jobs);
        assert_eq!(g.hs.slot.map(|s| s.1), Some(want), "jobs={jobs} ncpu={ncpu} mem_kb={mem_kb}");
        g.finish();
    }
    let _ = std::fs::remove_dir_all(base);
}

/// #1956：共用 target 是一個池（份數＝名額數）。第一份被佔著時下一個呼叫拿第二份——第一次用到時從第一份複製出來
/// （連同 `.fingerprint` 與 source manifest），不是冷編譯；之後那份留著各自增量，不再重新複製。
#[test]
fn a_busy_shared_target_hands_the_next_call_a_seeded_copy_instead_of_a_cold_dir() {
    let base = base();
    let pool = Admission { max: 3, pool: 0, ..QUICK };
    let mut a = guard_with(&base, HASH, "job-1-1", pool);
    let adir = PathBuf::from(&a.hs.dir);
    assert!(a.hs.dir.ends_with("/shared"), "{:?}", a.hs);
    std::fs::create_dir_all(adir.join("target/debug/.fingerprint/p-1")).unwrap();
    std::fs::create_dir_all(adir.join("target/debug/deps")).unwrap();
    std::fs::write(adir.join("target/debug/.fingerprint/p-1/lib-p"), "fp").unwrap();
    std::fs::write(adir.join("target/debug/deps/libp.rlib"), "old").unwrap();
    std::fs::write(adir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(format!("{}.source-manifest", a.hs.dir), "m").unwrap();

    let mut b = guard_with(&base, HASH, "job-2-2", pool);
    let bdir = PathBuf::from(&b.hs.dir);
    assert!(b.hs.dir.ends_with(&format!("/{HASH}/shared-2")), "第一份在用：拿池裡的第二份，不是 job 目錄：{:?}", b.hs);
    assert_eq!(std::fs::read_to_string(bdir.join("target/debug/deps/libp.rlib")).unwrap(), "old", "從第一份複製了產物");
    assert_eq!(std::fs::read_to_string(bdir.join("target/debug/.fingerprint/p-1/lib-p")).unwrap(), "fp", "指紋也一起複製");
    assert!(bdir.join("Cargo.toml").is_file(), "來源也一起");
    assert_eq!(std::fs::read_to_string(format!("{}.source-manifest", b.hs.dir)).unwrap(), "m", "manifest 跟著複製，失效 touch 只碰差異");
    assert!(!Path::new(&format!("{}.seed", b.hs.dir)).exists(), "暫存的 .seed 改名成正式的了");
    std::fs::write(bdir.join("target/own"), "b").unwrap();
    assert!(b.finish().unwrap().success());
    assert!(bdir.join("target/own").is_file(), "池裡的每一份都留著給下一次（不是用完就刪的 job 目錄）");
    assert!(a.finish().unwrap().success());

    // 都空著：照順序先拿第一份；第一份再被佔著時拿到的是已經在的第二份，不會重新複製蓋掉它。
    std::fs::write(adir.join("target/debug/deps/libp.rlib"), "new").unwrap();
    let mut c = guard_with(&base, HASH, "job-3-3", pool);
    assert_eq!(c.hs.dir, adir.display().to_string());
    let mut d = guard_with(&base, HASH, "job-4-4", pool);
    assert_eq!(d.hs.dir, bdir.display().to_string());
    assert!(bdir.join("target/own").is_file(), "已經在的那份不重新複製");
    assert_eq!(std::fs::read_to_string(bdir.join("target/debug/deps/libp.rlib")).unwrap(), "old");
    d.finish();
    c.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// #1956：池全忙時每秒輪詢**每一份**，任何一份空出來就拿——不是只等第一份。
#[test]
fn a_full_pool_hands_out_whichever_copy_frees_up_first() {
    let base = base();
    let pool = Admission { max: 2, pool: 0, ..QUICK };
    let mut a = guard_with(&base, HASH, "job-1-1", pool);
    let mut b = guard_with(&base, HASH, "job-2-2", pool);
    assert!(b.hs.dir.ends_with("/shared-2"), "{:?}", b.hs);
    let bdir = b.hs.dir.clone();

    // 兩份都在用：同步點是排隊中的心跳 `Q`
    let shared_wait = 30;
    let adm_c = Admission { shared_wait_secs: shared_wait, max: 3, pool: 2, ..QUICK };
    let c_token = LeaseToken::new();
    let mut waiter = guard_cmd(&base, HASH, "job-3-3", &c_token, adm_c)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(waiter.stdout.take().unwrap());
    wait_for_guard_queued(&mut out, shared_wait);

    b.finish();
    let (hs, _) = read_handshake_observed(&mut out);
    let c_hs = hs.expect("第二份空出來 waiter 要拿到");
    assert_eq!(c_hs.dir, bdir, "拿到先空出來的那份（第二份），不是死等第一份");

    finish_waiter(waiter, shared_wait);
    a.finish();
    let _ = std::fs::remove_dir_all(base);
}

/// 池的其他份（`shared-<N>`）跟 `shared` 一樣算共用 target：回收照共用的門檻、計進 `max_shared_dirs`；長得像但不是的不碰。
#[test]
fn pool_copies_are_shared_targets_for_gc() {
    assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-2"), Some(LeafKind::Shared));
    assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-12"), Some(LeafKind::Shared));
    assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-"), None);
    assert_eq!(leaf_kind("/r", "/r/0123456789abcdef/shared-2.seed"), None);
    let dir = |leaf: &str, idle_secs| RemoteDir { path: format!("/r/0123456789abcdef/{leaf}"), locked: false, busy: false, idle_secs };
    let hs = Handshake {
        root: "/r".into(),
        dir: "/r/0123456789abcdef/shared-3".into(),
        dirs: vec![dir("shared", ORPHAN_IDLE_SECS + 5), dir("shared-2", ORPHAN_IDLE_SECS + 10)],
        ..Default::default()
    };
    let policy = GcPolicy { shared_idle_secs: 24 * 3600, max_shared: 2 };
    assert_eq!(select_gc(&hs, &policy), vec!["/r/0123456789abcdef/shared-2".to_string()], "連自己這份共 3 份、上限 2：收最久沒用的");
}
