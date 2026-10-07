
    //! Runs the real script against a fake `cargo`/`curl`, mirroring `herdr_shim.rs`'s Sandbox.
    use std::io::Write as _;
    use std::process::Command;

    /// 剛寫好的腳本立刻 `exec` 可能撞上 `ETXTBSY`（issue #189）：共用 [`crate::exec_retry`]，遇到才重試。
    fn output_retrying(cmd: &mut Command) -> std::process::Output {
        crate::exec_retry::output(cmd).unwrap()
    }

    fn spawn_retrying(cmd: &mut Command) -> std::process::Child {
        crate::exec_retry::spawn(cmd).unwrap()
    }

    /// 寫測試用腳本並確定它已經可以被 exec（`content` 以 `#!` 開頭）：見 [`crate::testing::write_exec`]。
    fn write_exec(path: impl AsRef<std::path::Path>, content: impl AsRef<str>) {
        crate::testing::write_exec(path, content)
    }

    struct Sandbox {
        dir: std::path::PathBuf,
        /// 這個沙盒起過的每一次 shim 的 process group（issue #151）：Drop（含測試 panic）時整組終止。
        groups: std::sync::Mutex<Vec<i32>>,
    }

    /// 這個 process group 裡還活著的行程（`pid`、指令），殭屍不算。
    fn group_members(pgid: i32) -> Vec<(i32, String)> {
        let out = Command::new("ps").args(["-A", "-o", "pid=,pgid=,stat=,command="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let pid: i32 = it.next()?.parse().ok()?;
                let g: i32 = it.next()?.parse().ok()?;
                let stat = it.next()?;
                (g == pgid && !stat.starts_with('Z')).then(|| (pid, it.collect::<Vec<_>>().join(" ")))
            })
            .collect()
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // 修正前的 shim 不會停掉它們：別讓失敗的測試留下五分鐘的孤兒 sleep。
            for f in ["cargo.pid", "rustc.pid"] {
                if let Some(pid) = self.pid(f) {
                    self.kill_if_ours(pid);
                }
            }
            for pid in self.spawned() {
                self.kill_if_ours(pid);
            }
            let groups = self.groups.lock().unwrap().clone();
            for g in groups {
                self.kill_group_if_ours(g);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cargo-shim-{}", crate::db::ulid())));
            std::fs::create_dir_all(&dir).unwrap();
            super::install_local(&dir).unwrap();
            let fake = dir.join("real");
            std::fs::create_dir_all(&fake).unwrap();
            // Echoes argv and the env vars a test cares about, one per line, so assertions don't need a
            // real compiler. `$AM_TEST_FAKE_CARGO_LOG` records that the real cargo actually ran.
            write_exec(
                &fake.join("cargo"),
                "#!/bin/sh\n\
                 { printf 'CARGO_BUILD_JOBS=%s\\n' \"${CARGO_BUILD_JOBS:-}\"; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; } \
                   >> \"${AM_TEST_FAKE_CARGO_LOG:-/dev/null}\"\n\
                 exit \"${AM_TEST_FAKE_CARGO_EXIT:-0}\"\n",
            );
            // shim 自己也是剛寫好的腳本：用一個沒有副作用的呼叫（輕量子指令、真 cargo 換成 /usr/bin/true）exec 到成功為止。
            let probe = output_retrying(
                Command::new(dir.join("bin/cargo"))
                    .arg("--version")
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("AM_REAL_CARGO", "/usr/bin/true"),
            );
            assert!(probe.status.success(), "{:?}", probe);
            Sandbox { dir, groups: Default::default() }
        }

        /// `AM_TEST_CURL_SCRIPT` is the fake curl's own body (appended after the shebang); tests write
        /// whatever behavior they need (record calls, answer with canned JSON, fail to simulate no daemon).
        fn install_fake_curl(&self, body: &str) {
            let path = self.dir.join("real").join("curl");
            write_exec(&path, format!("#!/bin/sh\n{body}\n"));
        }

        /// 乾淨的一次 shim 呼叫：**呼叫端所有的 `AM_*` 都清掉**，不是列一份清單。這些測試跑在
        /// bot 的 pane 裡時，環境本來就有 `AM_DAEMON_EXE`／`AM_CONFIG_PATH`／`AM_DATA_DIR`（外部
        /// 編譯）、`AM_BOT_ID`、`AM_REAL_CARGO`…；漏進來的那一個就讓「假設某變數沒設」的測試必定
        /// 失敗，或讓 check 真的被 offload 到遠端主機（2026-09-19 build child 部署時兩種都中）。
        /// 名單永遠會漏，字首才不會；需要值的測試自己設。
        fn command(&self, path: &str) -> Command {
            self.command_in(None, path)
        }

        /// 同上，可以指定用哪個 shell 跑 shim（macOS 的 `/bin/sh`、`/bin/bash` 是 3.2，另有 `/bin/dash`）；`None` 照 shebang。
        fn command_in(&self, shell: Option<&str>, path: &str) -> Command {
            let mut cmd = match shell {
                Some(sh) => {
                    let mut c = Command::new(sh);
                    c.arg(self.dir.join("bin/cargo"));
                    c
                }
                None => Command::new(self.dir.join("bin/cargo")),
            };
            cmd.env("PATH", path);
            for (key, _) in std::env::vars() {
                if key.starts_with("AM_") {
                    cmd.env_remove(key);
                }
            }
            // 假的 $HOME：不能真的去讀開發機自己的 ui-token（會讓測試偷偷通過或偷偷失敗）。
            cmd.env("HOME", self.dir.join("fake-home"));
            // 這支測試本身就在 `cargo test` 底下跑（repo 的 `.cargo/config.toml [env]` 或外層 shim 給的值，issue #813）：
            // 漏進來就變成「呼叫端自己設了」，shim 不會再注入。需要的測試自己設。
            cmd.env_remove("RUST_TEST_THREADS");
            cmd
        }


        /// 假 cargo 把自己與它的「rustc」子行程的 pid 寫在沙盒裡，跑到 `secs` 秒才寫 `cargo.done` 並正常結束。
        /// 子行程是一顆獨立的 `sleep 300`：cargo 死了它也不會自己結束，正好用來抓「留下孤兒編譯器」。
        /// 它的 stdout／stderr 導去 /dev/null——不然它握著測試捕捉輸出的 pipe，cargo 正常跑完時 `.output()` 會等到它結束。
        fn install_slow_cargo(&self, secs: u32) {
            let d = self.dir.display();
            let body = format!(
                "#!/bin/sh\n[ -z \"${{AM_TEST_FAST:-}}\" ] || exit 0\necho $$ > '{d}/cargo.pid'\n( exec /bin/sleep 300 ) >/dev/null 2>&1 &\necho $! > '{d}/rustc.pid'\n/bin/sleep {secs} &\nw=$!\ntouch '{d}/cargo.ready'\nwait $w\nkill $(cat '{d}/rustc.pid') 2>/dev/null\necho done > '{d}/cargo.done'\nexit 0\n"
            );
            let path = self.dir.join("real/cargo");
            write_exec(&path, body);
        }

        /// 一直跑到**虛擬時鐘**走過 `virtual_secs` 秒才正常結束（寫 `cargo.done`、退 0）的假 cargo（要先 `install_virtual_clock`）。
        /// 「租約撐過好幾個 TTL」這類斷言要的是「虛擬時間走過去了」而不是「真的過了幾秒」——用固定的真實秒數
        /// （`install_slow_cargo(3)`）時，機器一忙守衛在那 3 秒裡跑不完足夠的輪數，斷言就間歇紅（issue #189）。
        /// 真實時間只留一個很寬的保險上限（75 秒），超過就退 3，讓測試明確失敗而不是永遠等。
        fn install_cargo_until_virtual(&self, virtual_secs: u32) {
            let d = self.dir.display();
            write_exec(
                self.dir.join("real/cargo"),
                format!(
                    "#!/bin/sh\necho $$ > '{d}/cargo.pid'\ntouch '{d}/cargo.ready'\nstart=$(cat '{d}/clock')\ni=0\nwhile [ $(( $(cat '{d}/clock') - start )) -lt {virtual_secs} ] && [ $i -lt 1500 ]; do /bin/sleep 0.05; i=$((i + 1)); done\n[ $(( $(cat '{d}/clock') - start )) -ge {virtual_secs} ] || exit 3\necho done > '{d}/cargo.done'\nexit 0\n"
                ),
            );
        }

        /// 名額租約的假 daemon：acquire 一律 granted（TTL 由參數給），renew 依 `renew-mode` 檔決定：
        /// `ok`＝續約成功、`notfound`／`mismatch`＝明確拒絕、`down`＝連不上（curl 退 7）、
        /// `once_down`＝第一次連不上、之後都成功，`alternate`＝一次失敗、一次成功、交替下去。每一通都記在 `curl.log`。
        fn install_lease_curl(&self, ttl: u32) {
            let d = self.dir.display();
            self.install_fake_curl(&format!(
                r#"echo "$*" >> '{d}/curl.log'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":{ttl}}}' ;;
  *renew*)
    case "$(cat '{d}/renew-mode')" in
      ok) printf '{{"renewed":true,"expires_at":"x"}}' ;;
      notfound) printf '{{"error":"not_found","what":"build_slot"}}' ;;
      mismatch) printf '{{"error":"token_mismatch"}}' ;;
      down) exit 7 ;;
      once_down) if [ -f '{d}/once' ]; then printf '{{"renewed":true,"expires_at":"x"}}'; else touch '{d}/once'; exit 7; fi ;;
      success_then_fail_until_*) if [ ! -f '{d}/renew-succeeded' ]; then touch '{d}/renew-succeeded'; printf '{{"renewed":true,"expires_at":"x"}}'; else lim=$(cat '{d}/renew-mode'); lim=${{lim#success_then_fail_until_}}; if [ $(( $(cat '{d}/clock') - 1000000 )) -lt "$lim" ]; then exit 7; else printf '{{"renewed":true,"expires_at":"x"}}'; fi; fi ;;
      alternate) if [ -f '{d}/flip' ]; then rm -f '{d}/flip'; printf '{{"renewed":true,"expires_at":"x"}}'; else touch '{d}/flip'; exit 7; fi ;;
      fail_until_*) lim=$(cat '{d}/renew-mode'); lim=${{lim#fail_until_}}; if [ $(( $(cat '{d}/clock') - 1000000 )) -lt "$lim" ]; then exit 7; else printf '{{"renewed":true,"expires_at":"x"}}'; fi ;;
    esac ;;
  *) printf '{{}}' ;;
esac"#
            ));
            self.set_renew_mode("ok");
        }

        /// 虛擬時鐘：把 PATH 上的 `sleep`／`date` 換成假的。`sleep N` 只把時鐘往前撥 N 秒（真的只睡 50ms，讓別的行程有機會跑），
        /// `date +%s` 讀這個時鐘。租約守衛的決定（要不要停）只看這兩個，所以測試**不吃機器負載**、不用真的等 TTL——
        /// 用真時鐘＋幾秒的 TTL 時，餘裕只有一兩秒，本機同時有人在編譯就會誤判。
        /// 只有背景的守衛在睡覺，時鐘只有一個寫入者；但讀的人很多（假 cargo、假 `date`、測試本身），而且 shim 收尾時會
        /// SIGKILL 守衛正在跑的 `sleep`。所以時鐘（與 `date-count`）一律先寫暫存檔再 `mv` 換上去：`echo … > clock` 會先把檔案
        /// 截成 0 bytes 再寫，讀在那一瞬間就讀到空字串，寫入者剛好在那時被殺，時鐘就永遠是空的（issue #857）。
        fn install_virtual_clock(&self) {
            let d = self.dir.display();
            std::fs::write(self.dir.join("clock"), "1000000").unwrap();
            let files = [
                (
                    "sleep",
                    // 等假 cargo 發出 ready 才撥時鐘：不然守衛在 cargo 還沒起來時就先判完了，殺樹的測試會空過。
                    format!(
                        "#!/bin/sh\ni=0\nwhile [ ! -e '{d}/cargo.ready' ] && [ $i -lt 500 ]; do /bin/sleep 0.02; i=$((i + 1)); done\nn=$(cat '{d}/clock')\necho $((n + ${{1%%.*}})) > '{d}/clock.$$' && mv -f '{d}/clock.$$' '{d}/clock'\nexec /bin/sleep 0.05\n"
                    ),
                ),
                ("date", format!("#!/bin/sh\ncase \"$*\" in '+%s') n=$(cat '{d}/date-count' 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" > '{d}/date-count.$$' && mv -f '{d}/date-count.$$' '{d}/date-count'; if [ -f '{d}/date-fail-at' ] && [ \"$n\" -eq \"$(cat '{d}/date-fail-at')\" ]; then exit 1; fi; cat '{d}/clock' ;; *) exec /bin/date \"$@\" ;; esac\n")),
            ];
            for (name, body) in files {
                let path = self.dir.join("real").join(name);
                write_exec(&path, body);
            }
        }

        /// 虛擬時鐘走了幾秒。
        fn virtual_secs(&self) -> i64 {
            std::fs::read_to_string(self.dir.join("clock")).unwrap().trim().parse::<i64>().unwrap() - 1_000_000
        }

        fn set_renew_mode(&self, mode: &str) {
            std::fs::write(self.dir.join("renew-mode"), mode).unwrap();
        }

        fn fail_date_call(&self, call: u32) {
            std::fs::write(self.dir.join("date-fail-at"), call.to_string()).unwrap();
        }

        /// 一顆一直在生新「編譯器」的 cargo：兩個背景迴圈不停 fork 出 `sleep`（一個直接生、一個包一層子 shell），
        /// 每生一顆就把 pid 記進 `spawned.pids`。抓的是 cargo 在「拍行程快照」與「送訊號」之間又生出新行程的競態——
        /// 生出來的行程父親一死就被 init 收養，事後按父子關係再也追不到。
        fn install_spawning_cargo(&self) {
            let d = self.dir.display();
            let body = format!(
                "#!/bin/sh\necho $$ > '{d}/cargo.pid'\n\
                 ( while :; do /bin/sleep 300 >/dev/null 2>&1 & echo $! >> '{d}/spawned.pids'; /bin/sleep 0.02; done ) &\n\
                 ( while :; do ( /bin/sleep 302 >/dev/null 2>&1 & echo $! >> '{d}/spawned.pids'; wait ) & /bin/sleep 0.03; done ) &\n\
                 touch '{d}/cargo.ready'\n/bin/sleep 120\necho done > '{d}/cargo.done'\n"
            );
            let path = self.dir.join("real/cargo");
            write_exec(&path, body);
        }

        /// 假的 `remote-cargo` helper（`AM_DAEMON_EXE`）：先把收到的 argv 記進 `helper.log`，再照 `body` 跑。
        /// 回傳 pane 環境裡通常帶的三個變數（`AM_DAEMON_EXE`／`AM_CONFIG_PATH` 缺一個 shim 就不轉遠端；`AM_DATA_DIR` 可缺，issue #417）。
        fn install_fake_helper(&self, body: &str) -> Vec<(&'static str, String)> {
            let helper = self.dir.join("fake-helper");
            write_exec(&helper, format!("#!/bin/sh\necho \"$*\" >> '{}/helper.log'\n{body}\n", self.dir.display()));
            vec![("AM_DAEMON_EXE", helper.display().to_string()), ("AM_CONFIG_PATH", "/tmp/c.toml".into()), ("AM_DATA_DIR", "/tmp".into())]
        }

        /// 會數「現在有幾顆同時在跑」的假 cargo：起來就留一個記號、把當下的數量記進 `peak.log`，睡一秒才收掉記號。
        fn install_counting_cargo(&self) {
            let d = self.dir.display();
            let body = format!(
                "#!/bin/sh\nmkdir -p '{d}/running'\n: > '{d}/running/'$$\nls '{d}/running' | wc -l | tr -d ' ' >> '{d}/peak.log'\n/bin/sleep 1\nrm -f '{d}/running/'$$\nexit 0\n"
            );
            let path = self.dir.join("real/cargo");
            write_exec(&path, body);
        }

        /// PATH：沙盒的 shim、假的真 cargo／curl，再來才是系統的。
        fn path(&self) -> String {
            format!("{}:{}:/usr/bin:/bin", self.dir.join("bin").display(), self.dir.join("real").display())
        }

        /// 假 cargo 起來了（`cargo.ready` 出現）才往下；30 秒沒起來就讓測試失敗。
        fn wait_cargo_ready(&self) {
            let up = std::time::Instant::now();
            while !self.dir.join("cargo.ready").exists() {
                assert!(up.elapsed() < std::time::Duration::from_secs(30), "cargo 沒起來");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        /// 沙盒 `tmp/` 底下現有的租約狀態目錄（`am-cargo-lease.*`），排序過的名字。
        fn lease_dirs(&self) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(self.dir.join("tmp"))
                .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("am-cargo-lease.")).collect())
                .unwrap_or_default();
            v.sort();
            v
        }

        /// 這個 pid 現在所在的 process group（`ps`）；不在了回 `None`。
        fn pgid_of(pid: i32) -> Option<i32> {
            let out = Command::new("ps").args(["-p", &pid.to_string(), "-o", "pgid="]).output().ok()?;
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }

        /// 只在這個 pid **現在還在我們起過的 process group 裡**才送 KILL。
        ///
        /// pid／pgid 是全機共用、會回收的資源：整樹平行時光是 cargo_shim 的測試就每秒 fork 上萬次，macOS 的 pid 每隔幾秒就繞一圈。
        /// 記在檔案或清單裡的舊 pid 過一會兒可能已經是別人的（別的測試的 shim、甚至不相干的行程），盲目 `kill` 會把它們殺掉——
        /// 被殺的 shim 留下自己的續約守衛與 `sleep 60`（孤兒），整樹偶發紅（`a_guard_stopped_right_after_it_starts…`）。
        /// 送訊號前先確認它還在我們記下的某個 group 裡。
        fn kill_if_ours(&self, pid: i32) {
            if Self::pgid_of(pid).is_some_and(|g| self.groups.lock().unwrap().contains(&g)) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }

        /// [`Self::kill_if_ours`] 的 process group 版：組裡有任何成員的指令列含沙盒目錄才殺（shim 與它的守衛都是 `<沙盒>/bin/cargo`）。
        /// 整組都不含（例如只剩自己會結束的孤兒 `sleep`）就不動：那個 group id 可能已經是別人的。
        fn kill_group_if_ours(&self, pgid: i32) {
            let dir = self.dir.to_string_lossy();
            if group_members(pgid).iter().any(|(_, c)| c.contains(dir.as_ref())) {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        }

        /// `spawned.pids` 記下的所有 pid。
        fn spawned(&self) -> Vec<i32> {
            std::fs::read_to_string(self.dir.join("spawned.pids")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
        }

        fn pid(&self, file: &str) -> Option<i32> {
            std::fs::read_to_string(self.dir.join(file)).ok()?.trim().parse().ok()
        }

        /// 這顆 pid 還活著嗎（`kill -0`）。
        fn alive(&self, file: &str) -> bool {
            self.pid(file).is_some_and(|p| unsafe { libc::kill(p, 0) } == 0)
        }

        /// 起一個 shim，放進**自己的 process group**（issue #151）並登記：Drop（含測試 panic）時整組終止。
        /// stdout／stderr 寫進沙盒裡的檔案（不是 pipe：假 cargo 的子行程握著 pipe 會讓讀取端等到它結束）。
        fn start_group(&self, cmd: &mut Command, stdin: bool) -> (std::process::Child, std::path::PathBuf, std::path::PathBuf) {
            use std::os::unix::process::CommandExt as _;
            let tag = crate::db::ulid();
            let (out, err) = (self.dir.join(format!("out-{tag}")), self.dir.join(format!("err-{tag}")));
            cmd.process_group(0)
                .stdout(std::fs::File::create(&out).unwrap())
                .stderr(std::fs::File::create(&err).unwrap())
                .stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() });
            let child = spawn_retrying(cmd);
            self.groups.lock().unwrap().push(child.id() as i32);
            (child, out, err)
        }

        /// 跑一個 shim 並保證**不留行程**（issue #151）：有時間上限（卡住就整組殺掉並報錯，不是永遠等下去）、
        /// 結束後斷言組內一個行程都不剩（`sleep`、等名額的迴圈、假編譯器都算）。
        fn run_group(&self, mut cmd: Command, stdin: Option<&[u8]>) -> (String, String, i32) {
            let (mut child, out_path, err_path) = self.start_group(&mut cmd, stdin.is_some());
            let pgid = child.id() as i32;
            if let Some(data) = stdin {
                child.stdin.take().unwrap().write_all(data).unwrap();
            }
            let started = std::time::Instant::now();
            let status = loop {
                if let Some(st) = child.try_wait().unwrap() {
                    break st;
                }
                if started.elapsed() > std::time::Duration::from_secs(120) {
                    self.kill_group_if_ours(pgid);
                    panic!("shim 跑了 120 秒還沒結束（卡在等名額？）：{}", std::fs::read_to_string(&err_path).unwrap_or_default());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            self.assert_group_gone_noting(pgid, &format!("shim 的結束狀態：{status:?}\nshim 的 stderr：{}", std::fs::read_to_string(&err_path).unwrap_or_default()));
            (
                std::fs::read_to_string(&out_path).unwrap_or_default(),
                std::fs::read_to_string(&err_path).unwrap_or_default(),
                status.code().unwrap_or(-1),
            )
        }

        /// 模擬「整個 pane 被關」：對整組 SIGKILL，**補到組裡一個行程都不剩**（issue #379）。
        /// 單一一次 `killpg` 只送給當下的成員：某個成員正好在 fork 的那一瞬，子行程可能在訊號走完那份名單之後才進組
        /// （macOS 上實測約 144 次並行跑會踩到 1 次），父親死了、子行程（`sleep 120`）成了活到天荒地老的孤兒。
        /// 真的「關 pane」由終端一路殺到沒有為止；這條只補足測試的注入，不影響 `run_group` 對「shim 自己收乾淨」的斷言。
        fn kill_group(&self, pgid: i32) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                self.kill_group_if_ours(pgid);
                if group_members(pgid).is_empty() {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "整組補殺了 10 秒還沒空：{:?}", group_members(pgid));
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        /// 這個 process group 在幾秒內要一個行程都不剩；剩下的補殺並讓測試失敗（issue #151 的回歸）。
        fn assert_group_gone(&self, pgid: i32) {
            self.assert_group_gone_noting(pgid, "");
        }

        /// 同上；失敗訊息多帶 `note`（shim 是怎麼結束的：被訊號殺掉跟自己退出，原因完全不同）與那一組的完整 `ps`。
        fn assert_group_gone_noting(&self, pgid: i32, note: &str) {
            // 正常情況下一兩個 50ms 就空了；上限放寬到 40 秒是因為本機常常同時有很多人在編譯（行程表塞滿時 fork 都會失敗），
            // 只有真的留下行程的失敗路徑才會等滿（壓測 40 個並行 shim，最慢的要 7 秒才起來）。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
            loop {
                let left = group_members(pgid);
                if left.is_empty() {
                    return;
                }
                if std::time::Instant::now() > deadline {
                    let ps = Command::new("/bin/ps").args(["-o", "pid,ppid,pgid,stat,etime,wchan,command", "-g", &pgid.to_string()]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
                    self.kill_group_if_ours(pgid);
                    panic!("shim 結束後還留下行程（孤兒）：{left:?}\n{note}\n{ps}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
            self.run_in(None, env, args)
        }

        /// 指定用哪個 shell 跑 shim（macOS 內建 `/bin/sh` 與 `/bin/bash` 是 3.2、另有 `/bin/dash`）；
        /// `None` 就照 shebang。
        fn run_in(&self, shell: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
            let path = format!(
                "{}:{}:/usr/bin:/bin",
                self.dir.join("bin").display(),
                self.dir.join("real").display()
            );
            let mut cmd = self.command_in(shell, &path);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            self.run_group(cmd, None)
        }

        /// 模擬 kernel 的孤兒 process group 規則：**先凍住**（`am_kill_tree` 的第一輪 `kill -STOP` 之後、第二次行程快照
        /// `ps -A -o pid= -o ppid=`）才對整個 process group 送一次 SIGHUP，就像 kernel 發現「孤兒組裡有被 SIGSTOP 的成員」時做的。
        /// 順序要對：以前是在第一通 `ps -o ppid= -p` 就送，那時還沒凍住，而且假 `ps` 自己也在組內、被這個 HUP 打死，
        /// `am_kill_tree` 拿到空輸出就直接放棄——整棵樹只剩「HUP 對一直在 fork 的假 cargo 的那一下」能收，
        /// 剛 fork 出來、還沒被訊號掃到的 `sleep` 就成了孤兒（負載一高就踩到，issue #256）。
        /// 假 `ps` 先 `trap '' HUP`（忽略會被 exec 帶過去），自己不被打死、照樣把快照印出來。
        /// 真的孤兒組只在 runner（沒有控制終端、呼叫端不在同 session 的別組）上出現，本機重現不了，所以用假 `ps` 把那個訊號送出來。
        fn install_group_hup_on_first_freeze(&self) {
            let d = self.dir.display();
            write_exec(
                self.dir.join("real/ps"),
                format!(
                    "#!/bin/sh\ncase \"$*\" in\n  *-A*pid=*ppid=*)\n    n=$(cat '{d}/ps.snapshots' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{d}/ps.snapshots'\n    if [ \"$n\" = 2 ] && [ ! -e '{d}/hup.sent' ]; then : > '{d}/hup.sent'; trap '' HUP; kill -HUP 0; fi ;;\nesac\nexec /bin/ps \"$@\"\n"
                ),
            );
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

    /// issue #128（重開）：**受管的 bot**（`AM_BOT_ID`＋hook token＋`AM_PORT`）問不到排程器——連不上、回應是空的／不是 JSON／5xx——
    /// 不能悄悄變成沒有名額的 cargo：daemon 重啟／升級／DB 出問題的瞬間，所有 bot 同時開編就繞過了 `max_concurrent`。
    /// 有限次重試之後 fail closed（exit 75，可重試），cargo 一次都沒起來，stderr 講明怎麼明確繞過。每一種 shell 都驗（含 macOS 的 bash 3.2）。
    #[test]
    fn a_managed_bot_never_gets_an_unscheduled_cargo_when_the_scheduler_cannot_be_asked() {
        // (label, 假 curl 的本體)：連不上、空回應、HTML、5xx 的 JSON、看不懂的 JSON。
        let cases: [(&str, &str); 5] = [
            ("連不上", "exit 7\n"),
            ("空回應", "printf ''\n"),
            ("HTML", "printf '<html>502 Bad Gateway</html>'\n"),
            ("5xx", "printf '{\"error\":\"upstream: database is locked\"}'\n"),
            ("看不懂", "printf '{\"ok\":true}'\n"),
        ];
        for sh in shells() {
            for (label, curl) in cases {
                let s = Sandbox::new();
                s.install_fake_curl(curl);
                let log = s.dir.join("cargo.log");
                let mut env = lease_env(&s);
                env.push(("AM_BUILD_SCHEDULER_WAIT_SECS", "1".into())); // 不必真的等 120 秒
                env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
                let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["test", "-p", "agents-managerd"]);
                let why = format!("{sh} {label}: {err}");
                assert_eq!(rc, 75, "問不到排程器：受管的 bot 退 75（可重試），不是跑起來：{why}");
                assert!(!log.exists(), "真的 cargo 不能被叫起來（沒有名額就是突破 max_concurrent）：{why}");
                assert!(err.contains("受管的 bot 不會在沒有名額"), "要講明原因：{why}");
                assert!(err.contains("AM_CARGO_BYPASS_SCHEDULER"), "要講明怎麼明確繞過：{why}");
            }
        }
    }

    /// 暫時的：排程器前兩輪連不上、之後回來了——受管的 bot 每 3 秒重試，拿到名額才跑（不是先跑再說）。
    #[test]
    fn a_managed_bot_retries_until_the_scheduler_is_back_and_only_then_runs() {
        for sh in shells() {
            let s = Sandbox::new();
            let d = s.dir.display();
            s.install_fake_curl(&format!(
                "echo \"$*\" >> '{d}/curl.log'\ncase \"$*\" in\n  *acquire*)\n    n=$(cat '{d}/n' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{d}/n'\n    if [ \"$n\" -le 1 ]; then exit 7; fi\n    echo acquired >> '{d}/order.log'\n    printf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}' ;;\n  *) printf '{{}}' ;;\nesac\n"
            ));
            let order = s.dir.join("order.log");
            let mut env = lease_env(&s);
            env.push(("AM_BUILD_SCHEDULER_WAIT_SECS", "30".into()));
            env.push(("AM_TEST_FAKE_CARGO_LOG", order.display().to_string()));
            let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["check", "-p", "agents-managerd"]);
            let why = format!("{sh}: {err}");
            assert_eq!(rc, 0, "{why}");
            assert!(err.contains("每 3 秒重試"), "要講在重試：{why}");
            let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(str::to_string).collect();
            let pos = |w: &str| lines.iter().position(|l| l == w).unwrap_or_else(|| panic!("{why}: 順序紀錄裡沒有 {w}：{lines:?}"));
            assert!(pos("acquired") < pos("CARGO_BUILD_JOBS=2"), "拿到名額才起 cargo：{why} {lines:?}");
        }
    }

    /// 排程器回 `unauthorized`（bot 身分被拒）不會在幾秒內自己好：不等滿重試，馬上 fail closed（77），不是把認證失敗變成 bypass。
    #[test]
    fn a_rejected_bot_identity_stops_at_once_instead_of_waiting_or_bypassing() {
        let s = Sandbox::new();
        s.install_fake_curl("printf '{\"error\":\"unauthorized\",\"message\":\"need a matching X-AM-Bot-Token+bot_id, or X-AM-Token\"}'\n");
        let log = s.dir.join("cargo.log");
        let mut env = lease_env(&s);
        env.push(("AM_BUILD_SCHEDULER_WAIT_SECS", "120".into()));
        env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
        let t0 = std::time::Instant::now();
        let (_, err, rc) = s.run(&as_refs(&env), &["build"]);
        assert_eq!(rc, 77, "{err}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(60), "身分被拒不該等重試：{:?}", t0.elapsed());
        assert!(!log.exists(), "{err}");
        assert!(err.contains("unauthorized"), "{err}");
    }

    /// #460 的守衛回 `holder_bot_mismatch`（`<agent 名>:<pid>` 撞到別顆 bot 還沒過期的那一列）也不會在幾秒內
    /// 自己好：同 `unauthorized`，當場 fail closed（77），不落到「回應看不懂」那條等滿 120 秒——那個訊息看的人
    /// 也不知道發生什麼事（i406 的審核，Refs #460）。
    #[test]
    fn a_holder_owned_by_another_bot_stops_at_once_with_a_message_that_says_why() {
        let s = Sandbox::new();
        s.install_fake_curl("printf '{\"error\":\"forbidden\",\"reason\":\"holder_bot_mismatch\",\"message\":\"a bot may only reuse its own build slot holder\"}'\n");
        let log = s.dir.join("cargo.log");
        let mut env = lease_env(&s);
        env.push(("AM_BUILD_SCHEDULER_WAIT_SECS", "120".into()));
        env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
        let t0 = std::time::Instant::now();
        let (_, err, rc) = s.run(&as_refs(&env), &["build"]);
        assert_eq!(rc, 77, "{err}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(60), "不該等滿重試：{:?}", t0.elapsed());
        assert!(!log.exists(), "沒有名額就不能起 cargo：{err}");
        assert!(err.contains("holder_bot_mismatch"), "訊息要講得出是哪一種拒絕：{err}");
    }

    /// 人工 host shell（沒有 bot 身分）維持**明講的** bypass：排程器問不到就直接跑，stderr 說一聲。
    #[test]
    fn a_manual_shell_keeps_the_announced_bypass_when_the_scheduler_is_down() {
        let s = Sandbox::new();
        s.install_fake_curl("exit 7\n");
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "test-token").unwrap();
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(&[("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())], &["test", "-p", "agents-managerd"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("連不上") && err.contains("直接跑"), "{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("agents-managerd"));
    }

    /// bot 要繞過排程器得**明講**（`AM_CARGO_BYPASS_SCHEDULER=1`），不是連線錯誤自動取得的：明講的話 cargo 直接跑、一通 curl 都不叫。
    #[test]
    fn a_bot_can_bypass_the_scheduler_only_by_saying_so() {
        let s = Sandbox::new();
        s.install_fake_curl(&format!("echo \"$*\" >> '{}/curl.log'\nexit 7\n", s.dir.display()));
        let log = s.dir.join("cargo.log");
        let mut env = lease_env(&s);
        env.push(("AM_CARGO_BYPASS_SCHEDULER", "1".into()));
        env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
        let (_, err, rc) = s.run(&as_refs(&env), &["build"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("AM_CARGO_BYPASS_SCHEDULER"), "要講這次是明確繞過：{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("build"), "cargo 照跑");
        assert!(!s.dir.join("curl.log").exists(), "明確繞過就不問排程器");
    }

    /// 同一類的另兩個入口：受管的 bot 建不出暫存目錄（沒地方放 pid 與失效標記，守衛停不了 cargo）、或這台機器沒有 curl——
    /// 也是「排程器用不了」，一樣 fail closed（拿到的名額要放回去）；人工 shell 才直接跑。
    #[test]
    fn a_managed_bot_that_cannot_hold_or_ask_for_a_slot_is_stopped_too() {
        // 建不出暫存目錄：TMPDIR 指到不存在的地方；排程器本身是好的（granted），所以名額要放回去。
        let s = Sandbox::new();
        let d = s.dir.display();
        s.install_fake_curl(&format!(
            "echo \"$*\" >> '{d}/curl.log'\ncase \"$*\" in\n  *acquire*) printf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}' ;;\n  *) printf '{{}}' ;;\nesac\n"
        ));
        let log = s.dir.join("cargo.log");
        let mut env = lease_env(&s);
        env.retain(|(k, _)| *k != "TMPDIR");
        env.push(("TMPDIR", s.dir.join("no/such/dir").display().to_string()));
        env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
        let (_, err, rc) = s.run(&as_refs(&env), &["build"]);
        assert_eq!(rc, 75, "{err}");
        assert!(!log.exists(), "cargo 不能在沒有守衛的情況下跑：{err}");
        assert!(std::fs::read_to_string(s.dir.join("curl.log")).unwrap().contains("/release"), "拿到的名額要放回去");

        // 沒有 curl：PATH 只放 shim 需要的那幾個工具（不含 curl）。
        let s = Sandbox::new();
        let tools = s.dir.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        for t in ["head", "grep", "tr", "dirname", "cut", "cat", "sed", "awk", "ps", "rm", "mktemp", "sleep", "env", "sh", "date", "find", "wc", "uname", "printf", "kill", "tail"] {
            for base in ["/usr/bin", "/bin"] {
                let src = std::path::Path::new(base).join(t);
                if src.exists() {
                    let _ = std::os::unix::fs::symlink(&src, tools.join(t));
                    break;
                }
            }
        }
        let log = s.dir.join("cargo.log");
        let mut cmd = s.command(&format!("{}:{}:{}", s.dir.join("bin").display(), s.dir.join("real").display(), tools.display()));
        cmd.arg("build").env("AM_TEST_FAKE_CARGO_LOG", &log);
        for (k, v) in &lease_env(&s) {
            cmd.env(k, v);
        }
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 75, "{err}");
        assert!(err.contains("沒有 curl"), "{err}");
        assert!(!log.exists(), "{err}");
    }

    /// issue #195：cargo 的命令列是 `cargo [+toolchain] [全域旗標…] <子指令>`，子指令不一定是第一個參數。以前只看 `$1`：`cargo +nightly build`、
    /// `cargo -q build`、`cargo --locked test`、`cargo -Z … build` 一律被當成輕量子指令直接 exec，不問排程器。現在找出真正的子指令；
    /// 而且反過來寫（不在已知輕量清單裡的都當 heavy）：`.cargo/config.toml` 的 alias（本 repo 的 `cargo dev`）與自訂子指令（`cargo nextest`）也不再悄悄繞過。
    /// 每一種 shell 都驗（含 macOS 的 bash 3.2）：要排程的問過排程器且真的 cargo 收到原樣的參數；輕量的一通 curl 都不叫。
    #[test]
    fn the_subcommand_is_found_behind_a_toolchain_and_global_flags() {
        let heavy: &[&[&str]] = &[
            &["build"],
            &["+nightly", "build"],
            &["-q", "build"],
            &["--locked", "test", "-p", "agents-managerd"],
            &["-Z", "unstable-options", "build"],
            &["--color", "always", "check"],
            &["--color=never", "clippy"],
            &["+nightly", "-q", "--locked", "clippy", "--all-targets"],
            &["-C", "daemon", "test"],
            &["--config", "build.jobs=2", "build"],
            &["dev"],
            &["nextest", "run"],
        ];
        let light: &[&[&str]] = &[&["--version"], &["-V"], &["-q", "metadata"], &["+nightly", "fmt", "--check"], &["--locked", "tree"], &["--color", "always", "fetch"], &["+nightly"], &[]];
        for sh in shells() {
            for (asked, cases) in [(true, heavy), (false, light)] {
                for args in cases {
                    let s = Sandbox::new();
                    s.install_fake_curl(&format!(
                        "echo \"$*\" >> '{d}/curl.log'\ncase \"$*\" in\n  *acquire*) printf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}' ;;\n  *) printf '{{}}' ;;\nesac\n",
                        d = s.dir.display()
                    ));
                    let log = s.dir.join("cargo.log");
                    let mut env = lease_env(&s);
                    env.push(("AM_TEST_FAKE_CARGO_LOG", log.display().to_string()));
                    let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), args);
                    let why = format!("{sh} cargo {args:?}: {err}");
                    assert_eq!(rc, 0, "{why}");
                    let asked_now = std::fs::read_to_string(s.dir.join("curl.log")).unwrap_or_default().contains("/acquire");
                    assert_eq!(asked_now, asked, "{}問排程器：{why}", if asked { "該" } else { "不該" });
                    // 真的 cargo 收到的參數要原樣（`CARGO_BUILD_JOBS=` 那行之後一行一個）。
                    let ran: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().skip(1).map(str::to_string).collect();
                    assert_eq!(ran, args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), "{why}");
                }
            }
        }
    }

    /// issue #153：遠端主機上的 bot pane 沒有 `AM_PORT`（遠端沒有 daemon，也不開反向埠，SPEC §11.4；daemon 不注入），
    /// 但有 `AM_BOT_ID`／`AM_HOOK_TOKEN`。以前 shim 在 `AM_PORT` 缺席時預設打 `127.0.0.1:7788`——在遠端那是**那台機器自己**，
    /// 名額要求打到不知道是誰的東西上（那台剛好也跑一份 agents-manager 的話，還會打到別顆 daemon）。
    /// 現在有 bot 身分卻沒有 `AM_PORT` 就是「不知道 daemon 在哪」：一通 curl 都不打，stderr 明講缺 `AM_PORT`，cargo 直接跑
    /// （遠端那台的編譯本來就不受這台 daemon 的名額管）。每一種 shell 都驗（含 macOS 的 bash 3.2）。
    #[test]
    fn a_bot_pane_without_am_port_never_talks_to_loopback_7788() {
        for sh in shells() {
            for sub in ["build", "check", "test"] {
                let s = Sandbox::new();
                s.install_fake_curl(&format!(
                    "echo \"$*\" >> '{d}/curl.log'\nprintf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}'\n",
                    d = s.dir.display()
                ));
                let cargo_log = s.dir.join("cargo.log");
                let env = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap())];
                let (_, err, rc) = s.run_in(Some(sh), &env, &[sub, "-p", "agents-managerd"]);
                let why = format!("{sh} {sub}: {err}");
                assert_eq!(rc, 0, "{why}");
                assert!(!s.dir.join("curl.log").exists(), "不知道 daemon 在哪，就一通 curl 都不能打（尤其不是 127.0.0.1:7788）：{why}");
                assert!(err.contains("AM_PORT"), "要明講缺什麼：{why}");
                assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "cargo 照跑：{why}");
                assert!(!err.contains("外部編譯沒有啟用"), "這個 pane 連 daemon 都沒有，不必再吵外部編譯：{why}");
            }
        }
    }

    /// 反面：預設 7788 只留給**人工 host shell**（沒有 bot 身分、用 `~/.config/agents-manager/ui-token`，SPEC 寫明的預設埠）；
    /// 有 `AM_PORT` 的（本機 bot）照它走。
    #[test]
    fn a_manual_shell_keeps_the_documented_default_port_and_a_set_am_port_wins() {
        let s = Sandbox::new();
        s.install_fake_curl(&format!(
            "echo \"$*\" >> '{d}/curl.log'\nprintf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}'\n",
            d = s.dir.display()
        ));
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "test-token").unwrap();
        let curl_log = s.dir.join("curl.log");
        let acquire_url = || {
            let log = std::fs::read_to_string(&curl_log).unwrap_or_default();
            let _ = std::fs::remove_file(&curl_log);
            log.lines().find(|l| l.contains("/acquire")).map(str::to_string).unwrap_or_default()
        };
        let (_, err, rc) = s.run(&[], &["build"]);
        assert_eq!(rc, 0, "{err}");
        let url = acquire_url();
        assert!(url.contains("http://127.0.0.1:7788/build-slots/acquire"), "人工 shell 沒有 AM_PORT：照文件寫的預設埠：{url}");
        let (_, err, rc) = s.run(&[("AM_PORT", "4242")], &["build"]);
        assert_eq!(rc, 0, "{err}");
        let url = acquire_url();
        assert!(url.contains("http://127.0.0.1:4242/build-slots/acquire"), "有 AM_PORT 就照它：{url}");
        let (_, err, rc) = s.run(&[("AM_PORT", "4243"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok")], &["build"]);
        assert_eq!(rc, 0, "{err}");
        let request = acquire_url();
        assert!(request.contains("http://127.0.0.1:4243/"), "本機 bot 照 AM_PORT");
        assert!(request.contains("X-AM-Bot-Id: b1") && request.contains("X-AM-Bot-Token: tok"), "Bot 請求帶成對身分：{request}");
        assert!(!request.contains("X-AM-Token"), "Bot 不得同時帶共用 User token：{request}");

        let (_, err, rc) = s.run(
            &[("AM_PORT", "4244"), ("AM_BOT_ID", "b1"), ("AM_BOT_TOKEN", "api-token"), ("AM_HOOK_TOKEN", "old-hook-token")],
            &["build"],
        );
        assert_eq!(rc, 0, "{err}");
        let request = acquire_url();
        assert!(request.contains("X-AM-Bot-Token: api-token"), "新 API token 優先於舊 hook 環境：{request}");
        assert!(!request.contains("old-hook-token"), "{request}");
    }

    #[test]
    fn a_bot_without_its_token_does_not_downgrade_to_a_valid_ui_token() {
        let s = Sandbox::new();
        s.install_fake_curl("echo called >> \"$AM_TEST_CURL_LOG\"\nprintf '{\\\"granted\\\":true}'\n");
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "valid-ui-token").unwrap();
        let log = s.dir.join("cargo.log");
        let curl_log = s.dir.join("curl.log");
        let log_path = log.display().to_string();
        let curl_log_path = curl_log.display().to_string();
        let (_, err, rc) = s.run(
            &[("AM_BOT_ID", "b1"), ("AM_PORT", "1"), ("AM_TEST_FAKE_CARGO_LOG", &log_path), ("AM_TEST_CURL_LOG", &curl_log_path)],
            &["build"],
        );
        assert_eq!(rc, 75, "Bot proof 缺失時受管 build fail closed：{err}");
        assert!(err.contains("拒絕改用共用 User token"), "{err}");
        assert!(!curl_log.exists(), "不能改用 UI token 詢問 scheduler");
        assert!(!log.exists(), "缺 credential 時不能啟動 cargo");
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
            &[("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap())],
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

    /// issue #138：check／test／clippy 該轉到外部編譯主機、卻因為 pane 缺 `AM_DAEMON_EXE`／`AM_CONFIG_PATH`
    /// （或 helper 不能執行）而退回本機時，要講出來，不能靜默——靜默的結果就是整批子 agent 的
    /// 編譯都塞在本機排隊，沒有人知道 #104 根本沒生效。build 這類本來就不 offload 的不吵。
    /// `AM_DATA_DIR` 不是前提（issue #417），見下一條。
    #[test]
    fn falling_back_to_local_for_a_missing_offload_variable_says_which_one() {
        let s = Sandbox::new();
        s.install_fake_curl(
            r#"case "$*" in
  *acquire*) printf '{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":30}' ;;
  *) printf '{}' ;;
esac
"#,
        );
        let cargo_log = s.dir.join("cargo.log");
        let log = cargo_log.to_str().unwrap();
        fn base<'a>(log: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
            let mut env = vec![("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_TEST_FAKE_CARGO_LOG", log)];
            env.extend_from_slice(extra);
            env
        }
        // 一個真的能執行的假 helper（回 125＝退回本機；不能用 `/bin/true`——macOS 沒有這個路徑）。
        let helper = s.dir.join("fake-helper");
        write_exec(&helper, "#!/bin/sh\nexit 125\n");
        let helper_path = helper.to_str().unwrap();
        // 什麼都沒有：一次講清楚缺哪兩個。
        let (_, err, rc) = s.run(&base(log, &[]), &["check", "-p", "agents-managerd"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("外部編譯沒有啟用"), "{err}");
        for k in ["AM_DAEMON_EXE", "AM_CONFIG_PATH"] {
            assert!(err.contains(k), "要點名缺 {k}：{err}");
        }
        assert!(!err.contains("AM_DATA_DIR"), "AM_DATA_DIR 不是前提（#417）：{err}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "照樣在本機跑");
        // 只缺一個：只點那一個。
        let (_, err, _) = s.run(&base(log, &[("AM_DAEMON_EXE", helper_path), ("AM_DATA_DIR", "/tmp")]), &["clippy"]);
        assert!(err.contains("AM_CONFIG_PATH") && !err.contains("AM_DAEMON_EXE") && !err.contains("AM_DATA_DIR"), "{err}");
        // helper 路徑在、但不能執行（binary 被換掉／搬走）：也算缺。
        let (_, err, _) = s.run(&base(log, &[("AM_DAEMON_EXE", "/nonexistent/agents-managerd"), ("AM_CONFIG_PATH", "/tmp/c.toml"), ("AM_DATA_DIR", "/tmp")]), &["test"]);
        assert!(err.contains("AM_DAEMON_EXE"), "{err}");
        // build／run 本來就不 offload：不吵。
        let (_, err, _) = s.run(&base(log, &[]), &["build", "--release"]);
        assert!(!err.contains("外部編譯"), "{err}");
        // 兩個都齊：不提示（helper 會自己決定要不要轉；這裡假 helper 回 125＝退回本機）。
        let (_, err, rc) = s.run(&base(log, &[("AM_DAEMON_EXE", helper_path), ("AM_CONFIG_PATH", "/tmp/c.toml")]), &["check"]);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("外部編譯沒有啟用"), "齊全就不提示：{err}");
    }

    /// issue #417：`scripts/check.sh` 為了不讓 daemon 測試吃到 pane 注入的正式資料目錄而 `env -u AM_DATA_DIR`，
    /// shim 以前把 `AM_DATA_DIR` 當轉遠端的前提，於是每一次 `scripts/check.sh daemon` 都退回本機排那 2 個名額，
    /// 外部編譯主機整段閒著。改成：沒有 `AM_DATA_DIR` 照樣交給 helper，只是不帶 `--data-dir`（helper 從設定檔推）。
    ///
    /// 根因是「改 env 的人（check.sh）」跟「讀 env 的人（shim）」各改各的（同 #138），所以這條直接拿
    /// `check.sh` 裡**實際那一行**的 `env …` 前綴去跑 shim：要轉得到 helper，而且 helper 以下（含退回本機時
    /// 的 cargo）都看不到 `AM_DATA_DIR`——隔離那一半也釘住，免得有人用「刪掉 `-u AM_DATA_DIR`」來修。
    #[test]
    fn the_check_script_env_still_offloads_without_leaking_the_data_dir() {
        // 執行期讀，不 include_str!：那會讓 check.sh 變成 binary 的建置輸入（`persona::BUILD_INPUTS` 的測試會擋）。
        let check_sh = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/check.sh")).expect("讀 scripts/check.sh");
        let env_line = check_sh
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("env ") && l.contains("cargo test -p agents-managerd"))
            .expect("check.sh 要用 env … cargo test -p agents-managerd 跑 daemon 測試");
        let prefix: Vec<&str> = env_line.split(" cargo ").next().unwrap().split_whitespace().skip(1).collect();
        assert!(prefix.windows(2).any(|w| w == ["-u", "AM_DATA_DIR"]), "測試不能吃到正式資料目錄：{env_line}");

        let s = Sandbox::new();
        s.install_fake_curl(r#"printf '{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":30}'"#);
        let cargo_log = s.dir.join("cargo.log");
        // helper 記下自己看到的 AM_DATA_DIR 後回 125（退回本機），順便驗本機那顆 cargo 也看不到。
        let helper_env = s.install_fake_helper(&format!("echo \"data_dir=${{AM_DATA_DIR-unset}}\" >> '{}/helper.log'\nexit 125", s.dir.display()));
        let pane: Vec<(&str, &str)> = helper_env
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .chain([("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap())])
            .collect();
        assert!(pane.iter().any(|(k, _)| *k == "AM_DATA_DIR"), "pane 本來就有 AM_DATA_DIR");
        let mut cmd = Command::new("/usr/bin/env");
        cmd.args(&prefix).arg(s.dir.join("bin/cargo")).args(["test", "-p", "agents-managerd", "--locked"]);
        cmd.env("PATH", format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        for (key, _) in std::env::vars() {
            if key.starts_with("AM_") {
                cmd.env_remove(key);
            }
        }
        for (k, v) in &pane {
            cmd.env(k, v);
        }
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("外部編譯沒有啟用"), "check.sh 的環境要轉得到外部編譯：{err}");
        let helper = std::fs::read_to_string(s.dir.join("helper.log")).expect("helper 要被叫到");
        assert!(helper.contains("remote-cargo --config /tmp/c.toml --cwd"), "{helper}");
        assert!(!helper.contains("--data-dir"), "沒有 AM_DATA_DIR 就不帶 --data-dir：{helper}");
        assert!(helper.contains("data_dir=unset"), "helper 看不到正式資料目錄：{helper}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "helper 回 125 就在本機跑");

        // 有 AM_DATA_DIR（一般 pane 直接跑 cargo）照舊帶 --data-dir。
        let (_, err, rc) = s.run(&pane, &["clippy"]);
        assert_eq!(rc, 0, "{err}");
        assert!(std::fs::read_to_string(s.dir.join("helper.log")).unwrap().contains("--data-dir /tmp --cwd"));
    }

    /// issue #155：會轉到外部編譯主機的 check／test／clippy **不佔本機的建置名額**——本機名額管的是本機的 RAM，
    /// 遠端編譯不吃它。以前 shim 先拿名額、再決定轉遠端，`max_concurrent=2` 時就算全部走遠端也只能同時跑 2 個，
    /// 其餘的子 agent 在本機排隊。排程器在這裡一律回 granted（額滿的話舊 shim 會忙等到測試逾時），
    /// 所以斷言的是「**根本沒去問**」；helper 的結束碼原樣帶出去、不會再偷偷在本機重跑。
    #[test]
    fn a_command_that_goes_to_the_remote_host_never_asks_for_a_local_slot() {
        let cases: Vec<(&str, &str, i32)> = shells()
            .into_iter()
            .flat_map(|sh| ["check", "test", "clippy"].into_iter().map(move |sub| (sh, sub)))
            .flat_map(|(sh, sub)| [0, 101, 126].into_iter().map(move |rc| (sh, sub, rc)))
            .collect();
        for (sh, sub, helper_rc) in cases {
            let s = Sandbox::new();
            s.install_fake_curl(&format!(
                "echo \"$*\" >> '{d}/curl.log'\nprintf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30}}'\n",
                d = s.dir.display()
            ));
            let remote = s.install_fake_helper(&format!("exit {helper_rc}"));
            let cargo_log = s.dir.join("cargo.log");
            let mut env = lease_env(&s);
            env.extend(remote);
            env.push(("AM_TEST_FAKE_CARGO_LOG", cargo_log.display().to_string()));
            let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &[sub, "-p", "agents-managerd"]);
            let why = format!("{sh} {sub}／helper 退 {helper_rc}：{err}");
            assert_eq!(rc, helper_rc, "遠端的結果原樣帶出去：{why}");
            let helper_log = std::fs::read_to_string(s.dir.join("helper.log")).unwrap_or_default();
            assert!(helper_log.contains("remote-cargo") && helper_log.contains(sub), "helper 要被叫到：{why}\n{helper_log}");
            let curl_log = std::fs::read_to_string(s.dir.join("curl.log")).unwrap_or_default();
            assert!(curl_log.is_empty(), "遠端編譯不該跟本機排程器要名額：{why}\n{curl_log}");
            assert!(!cargo_log.exists(), "轉到遠端的不該再在本機跑一次：{why}");
        }
    }

    /// 同一件事的另一面：helper 回 125（設定被關掉、不適合 offload）＝**沒有**在遠端跑，這時才真的落在本機——
    /// 也才去拿本機名額。順序是 helper → acquire → 本機 cargo → release。
    #[test]
    fn the_local_slot_is_only_taken_after_the_remote_host_declines() {
        let s = Sandbox::new();
        let order = s.dir.join("order.log");
        s.install_fake_curl(&format!(
            "case \"$*\" in\n  *acquire*) echo acquire >> '{o}'; printf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":3,\"lease_ttl_secs\":30}}' ;;\n  *release*) echo release >> '{o}'; printf '{{}}' ;;\n  *) printf '{{}}' ;;\nesac\n",
            o = order.display()
        ));
        let remote = s.install_fake_helper(&format!("echo helper >> '{}'\nexit 125", order.display()));
        let mut env = lease_env(&s);
        env.extend(remote);
        env.push(("AM_TEST_FAKE_CARGO_LOG", order.display().to_string()));
        let (_, err, rc) = s.run(&as_refs(&env), &["test", "-p", "agents-managerd"]);
        assert_eq!(rc, 0, "{err}");
        let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(str::to_string).collect();
        let pos = |what: &str| lines.iter().position(|l| l == what).unwrap_or_else(|| panic!("順序紀錄裡沒有 {what}：{lines:?}"));
        assert!(pos("helper") < pos("acquire"), "先問遠端、沒接才去拿本機名額：{lines:?}");
        assert!(pos("acquire") < pos("CARGO_BUILD_JOBS=3"), "拿到名額才在本機跑（jobs 數照名額回應）：{lines:?}");
        assert!(pos("CARGO_BUILD_JOBS=3") < pos("release"), "跑完才放：{lines:?}");
    }

    /// 真的 router（跟 daemon 同一份），回傳它的 port 與 server 的 handle。
    fn serve_router(app: std::sync::Arc<crate::state::App>) -> (u16, tokio::task::JoinHandle<()>) {
        serve_router_on(app, 0)
    }

    /// 同上，指定埠（0＝隨便一個）。
    fn serve_router_on(app: std::sync::Arc<crate::state::App>, port: u16) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let router = crate::api::router(app);
        let server = tokio::spawn(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            let _ = axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
        });
        (port, server)
    }

    /// 跑 `n` 顆 shim（同一個沙盒、同一個真的 router）：回傳它們的 process group 與 stderr 檔。
    fn start_shims(s: &Sandbox, port: u16, n: usize, extra: &[(&str, String)]) -> Vec<(std::process::Child, std::path::PathBuf)> {
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "test-token").unwrap();
        std::fs::create_dir_all(s.dir.join("tmp")).unwrap();
        (0..n)
            .map(|_| {
                let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
                cmd.env("AM_PORT", port.to_string()).env("TMPDIR", s.dir.join("tmp")).args(["test", "-p", "agents-managerd"]);
                for (k, v) in extra {
                    cmd.env(k, v);
                }
                let (child, _, err) = s.start_group(&mut cmd, false);
                (child, err)
            })
            .collect()
    }

    /// 等這批 shim 全部結束，回傳各自的結束碼與 stderr；一律確認整組行程都收乾淨。
    async fn finish_shims(s: &Sandbox, shims: Vec<(std::process::Child, std::path::PathBuf)>) -> Vec<(i32, String)> {
        let mut out = Vec::new();
        for (mut child, err) in shims {
            let started = std::time::Instant::now();
            let status = loop {
                if let Some(st) = child.try_wait().unwrap() {
                    break st;
                }
                assert!(started.elapsed() < std::time::Duration::from_secs(120), "shim 跑了 120 秒還沒結束：{}", std::fs::read_to_string(&err).unwrap_or_default());
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            };
            s.assert_group_gone(child.id() as i32);
            out.push((status.code().unwrap_or(-1), std::fs::read_to_string(&err).unwrap_or_default()));
        }
        out
    }

    /// issue #155 的驗收：本機名額 `max_concurrent=2`，同時進來 4 個會轉遠端的 test——**四個都要同時在遠端跑**，
    /// 名額表上一列都沒有（沒佔、也沒在排隊）。**真的** router＋**真的** curl＋**真的** shim。
    /// helper 起來就留記號、等測試說 go 才結束（等不到就退 3）：以前只有 2 個拿得到本機名額，另外 2 個 helper 根本沒被叫起來，
    /// 測試在「同時起來的只有 2 個」這一步就紅。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn many_remote_tests_at_once_are_not_limited_by_the_local_slots() {
        use std::time::{Duration, Instant};
        const N: usize = 4;
        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 2;
                Ok(())
            })
            .await
            .unwrap();
        let (port, server) = serve_router(env.app.clone());

        let s = Sandbox::new();
        let d = s.dir.display().to_string();
        let remote = s.install_fake_helper(&format!(
            "touch '{d}/started.'$$\ni=0\nwhile [ ! -e '{d}/go' ] && [ $i -lt 600 ]; do /bin/sleep 0.05; i=$((i + 1)); done\n[ -e '{d}/go' ] || exit 3\nexit 0"
        ));
        let extra: Vec<(&str, String)> = remote.iter().map(|(k, v)| (*k, v.clone())).collect();
        let shims = start_shims(&s, port, N, &extra);

        // 四個 helper 同時在跑。
        let started = || std::fs::read_dir(&s.dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("started.")).count();
        let up = Instant::now();
        while started() < N && up.elapsed() < Duration::from_secs(15) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let running = started();
        let st = crate::build_scheduler::status(&env.app).await.unwrap();
        std::fs::write(s.dir.join("go"), "").unwrap();
        let results = finish_shims(&s, shims).await;
        server.abort();
        assert_eq!(running, N, "本機名額只有 2 個，遠端編譯卻該同時跑 {N} 個（只起來 {running} 個）：{st}\n{results:?}");
        assert_eq!(st["active"], 0, "遠端編譯不該佔本機名額：{st}");
        assert!(st["slots"].as_array().unwrap().is_empty(), "也不該在本機排隊：{st}");
        for (rc, err) in &results {
            assert_eq!(*rc, 0, "{err}");
        }
    }

    /// 反面：遠端全都回 125（沒有轉成）時，4 個 test 才真的落在本機——本機名額照樣把**本機**同時在跑的編譯壓在 `max_concurrent=2`。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tests_that_end_up_on_this_machine_still_respect_the_local_slots() {
        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 2;
                Ok(())
            })
            .await
            .unwrap();
        let (port, server) = serve_router(env.app.clone());

        let s = Sandbox::new();
        s.install_counting_cargo();
        let remote = s.install_fake_helper("exit 125");
        let extra: Vec<(&str, String)> = remote.iter().map(|(k, v)| (*k, v.clone())).collect();
        let shims = start_shims(&s, port, 4, &extra);
        let results = finish_shims(&s, shims).await;
        server.abort();
        for (rc, err) in &results {
            assert_eq!(*rc, 0, "{err}");
        }
        let peaks: Vec<u32> = std::fs::read_to_string(s.dir.join("peak.log")).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
        assert_eq!(peaks.len(), 4, "四個都要跑完：{peaks:?}");
        assert_eq!(peaks.iter().max(), Some(&2), "本機同時在跑的編譯要剛好壓在 2（不是 1＝排太嚴、也不是 3+＝名額沒管到）：{peaks:?}");
    }

    /// issue #813 的驗收：名額用滿時，第 N+1 支 `cargo test` 的**執行階段**（test binary 在跑的時候）也要等，不能跟前面的同時跑；
    /// 而且每支 test binary 都拿到名額回應的 `RUST_TEST_THREADS`。**真的** router（`max_concurrent=1`、`test_threads=3`）＋真的 curl＋真的 shim。
    /// 假 cargo 先「編譯」一下，再起一顆獨立的「test binary」子行程跑一秒、等它結束才退出——跟真的 `cargo test` 一樣，名額要罩到 test binary 跑完。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_next_cargo_test_waits_until_the_previous_test_binary_has_finished_and_gets_the_thread_cap() {
        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 1;
                c.build.test_threads = 3;
                Ok(())
            })
            .await
            .unwrap();
        let (port, server) = serve_router(env.app.clone());

        let s = Sandbox::new();
        let d = s.dir.display();
        write_exec(
            s.dir.join("real/testbin"),
            format!(
                "#!/bin/sh\nmkdir -p '{d}/running'\n: > '{d}/running/'$$\necho \"$(ls '{d}/running' | wc -l | tr -d ' ') RUST_TEST_THREADS=${{RUST_TEST_THREADS-unset}}\" >> '{d}/testbin.log'\n/bin/sleep 1\nrm -f '{d}/running/'$$\n"
            ),
        );
        write_exec(s.dir.join("real/cargo"), format!("#!/bin/sh\n/bin/sleep 0.2\n'{d}/real/testbin'\n"));
        let shims = start_shims(&s, port, 3, &[]);
        let results = finish_shims(&s, shims).await;
        server.abort();
        for (rc, err) in &results {
            assert_eq!(*rc, 0, "{err}");
        }
        let log = std::fs::read_to_string(s.dir.join("testbin.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 3, "三支都要跑完：{log}");
        for l in &lines {
            assert_eq!(*l, "1 RUST_TEST_THREADS=3", "test binary 在跑時不能有別支同時在跑，且要拿到名額的執行緒上限：{log}");
        }
    }

    /// issue #813：`RUST_TEST_THREADS` 只注入 `test`／`t`，而且只在呼叫端沒設的時候；名額回應沒有這個欄位（舊 daemon）＝預設 8，
    /// 回 `0`＝daemon 明講不設。
    #[test]
    fn the_test_thread_cap_follows_the_grant_and_never_overrides_the_caller() {
        let s = Sandbox::new();
        let d = s.dir.display();
        write_exec(s.dir.join("real/cargo"), format!("#!/bin/sh\necho \"$1 RUST_TEST_THREADS=${{RUST_TEST_THREADS-unset}}\" >> '{d}/rtt.log'\n"));
        let managed = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];
        let cases: [(&str, &[(&str, &str)], &str, &str); 6] = [
            (r#","test_threads":4"#, &[], "test", "test RUST_TEST_THREADS=4"),
            (r#","test_threads":4"#, &[], "t", "t RUST_TEST_THREADS=4"),
            (r#","test_threads":4"#, &[("RUST_TEST_THREADS", "16")], "test", "test RUST_TEST_THREADS=16"),
            (r#","test_threads":4"#, &[], "check", "check RUST_TEST_THREADS=unset"),
            (r#","test_threads":0"#, &[], "test", "test RUST_TEST_THREADS=unset"),
            ("", &[], "test", "test RUST_TEST_THREADS=8"),
        ];
        for (field, extra, sub, want) in cases {
            let _ = std::fs::remove_file(s.dir.join("rtt.log"));
            s.install_fake_curl(&format!(
                "case \"$*\" in *acquire*) printf '{{\"granted\":true,\"token\":\"tok-1\",\"cargo_jobs\":2,\"lease_ttl_secs\":30{field}}}' ;; *) printf '{{}}' ;; esac"
            ));
            let mut env: Vec<(&str, &str)> = managed.to_vec();
            env.extend_from_slice(extra);
            let (_, err, rc) = s.run(&env, &[sub, "-p", "agents-managerd"]);
            assert_eq!(rc, 0, "{field} {sub}: {err}");
            let got = std::fs::read_to_string(s.dir.join("rtt.log")).unwrap();
            assert_eq!(got.trim(), want, "回應 `{field}`、呼叫端 {extra:?}、子指令 {sub}");
        }
    }

    /// issue #128（重開）的驗收：`max_concurrent=1`，排程器整個沒開（沒有人在聽那個埠），同時起兩顆**受管的** bot 的 cargo——
    /// 撐過整段 outage，任何時刻都不能有 cargo 在跑（沒有名額）；排程器回來之後照常排隊，兩顆各自跑完、同時在跑的最多 1 顆。
    /// **真的** router（同 daemon 一份）＋**真的** curl＋**真的** shim；bot 身分是真的資料列（router 用 hook token 認證）。
    /// 以前 curl 連不上就 `exec` 真的 cargo：兩顆一起編。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scheduler_that_is_down_never_lets_two_managed_cargos_compile_and_the_queue_resumes() {
        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 1;
                Ok(())
            })
            .await
            .unwrap();
        let bot_id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok-128',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(format!("bot-{bot_id}"))
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        // 先佔一個埠再放掉：這段時間沒有人在聽，連線被拒（curl 7）。
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();

        let s = Sandbox::new();
        s.install_counting_cargo();
        let extra: Vec<(&str, String)> = vec![("AM_BOT_ID", bot_id.clone()), ("AM_HOOK_TOKEN", "tok-128".into()), ("AM_BUILD_SCHEDULER_WAIT_SECS", "90".into())];
        let shims = start_shims(&s, port, 2, &extra);

        // outage 期間（兩顆都至少重試過一輪）：沒有任何 cargo 起來。
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert!(!s.dir.join("peak.log").exists(), "排程器沒開、沒有名額：不能有 cargo 在跑：{:?}", std::fs::read_to_string(s.dir.join("peak.log")));

        // 排程器回來：照常排隊，一次一顆。
        let (_, server) = serve_router_on(env.app.clone(), port);
        let results = finish_shims(&s, shims).await;
        server.abort();
        for (rc, err) in &results {
            assert_eq!(*rc, 0, "{err}");
            assert!(err.contains("受管的 bot 不會在沒有名額時跑 cargo"), "outage 期間要講在等：{err}");
        }
        let peaks: Vec<u32> = std::fs::read_to_string(s.dir.join("peak.log")).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
        assert_eq!(peaks.len(), 2, "兩顆都要跑完：{peaks:?}");
        assert_eq!(peaks.iter().max(), Some(&1), "同時在跑的最多 1 顆（max_concurrent=1）：{peaks:?}");
    }

    /// 這台機器上有的 shell（沒有的略過）。macOS 的 `/bin/sh`／`/bin/bash` 是 3.2——腳本改動要在那個版本也驗。
    /// 守衛測試每個執行緒跑幾次；`AM_TEST_GUARD_ITERS` 可以拉高（壓測找極低機率的競態用）。
    fn guard_iterations() -> usize {
        std::env::var("AM_TEST_GUARD_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(60)
    }

    fn shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/bash", "/bin/dash"].into_iter().filter(|p| std::path::Path::new(p).exists()).collect()
    }

    /// 租約守衛用的環境：有 bot 身分、暫存目錄指到沙盒裡（才看得出有沒有留下狀態目錄）。
    fn lease_env(s: &Sandbox) -> Vec<(&'static str, String)> {
        let tmp = s.dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        vec![("AM_BOT_ID", "b1".into()), ("AM_HOOK_TOKEN", "tok".into()), ("AM_PORT", "1".into()), ("TMPDIR", tmp.display().to_string())]
    }

    fn as_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
        v.iter().map(|(k, val)| (*k, val.as_str())).collect()
    }

    /// issue #128：名額是 TTL 租的，daemon 端停止續約超過 TTL 就把它收回、讓別人拿。持有者**活著但租約已失效**時
    /// 必須停止使用容量——以前續約迴圈把失敗全吞掉，前景的 cargo 照跑，於是 A 還在編、B 拿到同一個名額也開始編，
    /// `max_concurrent` 被突破。daemon **明確**說沒有這個名額（`not_found`）或 token 對不上（`token_mismatch`）：
    /// 名額已經不是我們的，整棵 cargo／rustc 行程樹一起停、退 75（可重試）、名額放掉、狀態目錄清乾淨。
    /// 每一種 shell 都驗（含 macOS 的 bash 3.2）。用虛擬時鐘（TTL 180 秒，第一次續約在 60 秒）。
    #[test]
    fn an_explicit_renew_refusal_stops_cargo_and_every_compiler_under_it() {
        let cases: Vec<(&str, &str)> = shells().into_iter().map(|sh| (sh, "notfound")).chain([("/bin/sh", "mismatch")]).collect();
        std::thread::scope(|scope| {
            for (sh, mode) in cases {
                scope.spawn(move || {
                    let s = Sandbox::new();
                    s.install_virtual_clock();
                    s.install_lease_curl(180);
                    s.install_slow_cargo(120);
                    s.set_renew_mode(mode);
                    let env = lease_env(&s);
                    let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["check", "-p", "agents-managerd"]);
                    let why = format!("{sh} {mode}: {err}");
                    assert_eq!(rc, 75, "被我們停掉的一律退 75（可重試）：{why}");
                    assert!(s.pid("cargo.pid").is_some() && s.pid("rustc.pid").is_some(), "cargo 得真的起來過，不然這條測試是空過：{why}");
                    assert!(err.contains("名額租約失效"), "要告訴使用者為什麼：{why}");
                    assert!(!s.dir.join("cargo.done").exists(), "cargo 不能跑完：{why}");
                    assert!(s.virtual_secs() < 90, "第一次續約（60 秒）就該停，不是等到 TTL：{}s {why}", s.virtual_secs());
                    assert!(!s.alive("cargo.pid"), "cargo 還活著：{why}");
                    assert!(!s.alive("rustc.pid"), "cargo 底下的編譯器還活著（孤兒）：{why}");
                    let calls = std::fs::read_to_string(s.dir.join("curl.log")).unwrap();
                    assert!(calls.contains("/release"), "名額要放掉：{calls}");
                    let leftovers: Vec<_> = std::fs::read_dir(s.dir.join("tmp")).unwrap().flatten().collect();
                    assert!(leftovers.is_empty(), "狀態目錄要清掉：{leftovers:?}");
                });
            }
        });
    }

    /// 停的是**整棵**行程樹，而且是「凍住再殺」：cargo 一直在生新的 rustc（兩個迴圈不停 fork），如果只是拍一張快照、
    /// 對快照裡的 pid 送訊號，快照之後才生出來的行程父親一死就被 init 收養、再也追不到——一顆孤兒編譯器。
    /// 先 `SIGSTOP` 凍住、重拍到不再長新的，才 `TERM`。每一種 shell 都驗；活著的一個都不能剩。
    #[test]
    fn a_cargo_that_keeps_spawning_compilers_is_stopped_without_orphans() {
        std::thread::scope(|scope| {
            for sh in shells() {
                scope.spawn(move || {
                    let s = Sandbox::new();
                    s.install_virtual_clock();
                    s.install_lease_curl(180);
                    s.install_spawning_cargo();
                    s.set_renew_mode("notfound");
                    let env = lease_env(&s);
                    let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["build"]);
                    assert_eq!(rc, 75, "{sh}: {err}");
                    let spawned = s.spawned();
                    assert!(spawned.len() >= 2, "{sh}: 假 cargo 應該已經生出一批行程：{spawned:?}");
                    let alive: Vec<i32> = spawned.iter().copied().filter(|p| unsafe { libc::kill(*p, 0) } == 0).collect();
                    assert!(alive.is_empty(), "{sh}: 孤兒編譯器還活著：{alive:?}（共生出 {}）", spawned.len());
                    assert!(!s.alive("cargo.pid"), "{sh}: cargo 還活著");
                });
            }
        });
    }

    /// 沒有控制終端、呼叫端早就走了（CI、launchd、`nohup`）：shim 那一組是孤兒 process group，而停 cargo 要先 SIGSTOP 凍住整棵樹——
    /// kernel 對「孤兒組裡有被停住的成員」會整組送 SIGHUP，shim 自己被打死，呼叫端拿到的是訊號死（-1／129）而不是可重試的 75，
    /// 名額與狀態目錄也沒收（GitHub runner 上間歇紅的原因）。這裡在停 cargo 時對整組送一次 HUP：shim 得照樣退 75、什麼都不留。
    /// 每一種 shell 都驗。
    #[test]
    fn a_group_wide_sighup_while_stopping_cargo_does_not_take_the_shim_down() {
        for sh in shells() {
            let s = Sandbox::new();
            s.install_virtual_clock();
            s.install_lease_curl(180);
            s.install_spawning_cargo();
            s.install_group_hup_on_first_freeze();
            s.set_renew_mode("notfound");
            let env = lease_env(&s);
            let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["build"]);
            assert!(s.dir.join("hup.sent").exists(), "{sh}: 這條測試要真的送出過 HUP，不然是空過：{err}");
            assert_eq!(rc, 75, "{sh}: 整組收到 HUP 也要退 75（可重試），不是被訊號打死：{err}");
            assert!(err.contains("名額租約失效"), "{sh}: 要告訴使用者為什麼：{err}");
            assert!(!s.alive("cargo.pid"), "{sh}: cargo 還活著");
            assert!(std::fs::read_to_string(s.dir.join("curl.log")).unwrap().contains("/release"), "{sh}: 名額要放掉");
            let leftovers: Vec<_> = std::fs::read_dir(s.dir.join("tmp")).unwrap().flatten().collect();
            assert!(leftovers.is_empty(), "{sh}: 狀態目錄要清掉：{leftovers:?}");
        }
    }

    /// 續約**一直**失敗（daemon 連不上）：不能等到 daemon 端的到期時間之後才動手——那時 B 可能已經拿到同一個名額。
    /// TTL 180 秒：在它到期**之前**就要把 A 停掉（續約在 60、120 秒失敗，第二次失敗時離到期只剩一分鐘、
    /// 已經趕不及再試一次，就現在停），而且行程樹一起停。
    #[test]
    fn a_renew_that_keeps_failing_stops_cargo_before_the_lease_can_be_reclaimed() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        s.set_renew_mode("down");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["test"]);
        assert_eq!(rc, 75, "{err}");
        assert!(s.pid("cargo.pid").is_some() && s.pid("rustc.pid").is_some(), "cargo 得真的起來過，不然這條測試是空過：{err}");
        let at = s.virtual_secs();
        assert!(at < 180, "daemon 在 180 秒收回名額——A 必須在那之前就停：{at}s\n{err}");
        assert!(at >= 120, "第一次失敗不是死刑（還有機會再試一次）：{at}s");
        assert!(!s.dir.join("cargo.done").exists());
        assert!(!s.alive("cargo.pid") && !s.alive("rustc.pid"), "cargo 與它的編譯器都要停");
    }

    /// 對抗式審查（整樹 `cargo test` 在高負載下十幾分鐘就被 SIGTERM，今天好幾顆 child 都是 rc=143）：daemon 在負載 70–90 的機器上
    /// 回應慢，續約請求逾時就算失敗。以前失敗之後要等**整個續約間隔**（TTL 的三分之一）才再試一次，TTL 180 秒時只有兩次機會
    /// （60、120 秒）：連兩次逾時，離保守的到期時間只剩一分鐘、「來不及再試」就把一個完全合法的建置整棵殺掉。
    /// 失敗之後要改成短間隔連續重試、用到「再試一次也來得及」的最後一刻才放棄——daemon 慢了兩分鐘但在到期前回來，建置不能死。
    #[test]
    fn a_daemon_that_is_unresponsive_for_two_renew_intervals_but_recovers_in_time_does_not_kill_the_build() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_cargo_until_virtual(3 * 180 + 60);
        s.set_renew_mode("fail_until_130");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["test"]);
        assert_eq!(rc, 0, "daemon 在 130 秒就回來了，離 180 秒的到期還早：{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "應該正常跑完");
        let renews = std::fs::read_to_string(s.dir.join("curl.log")).unwrap().lines().filter(|l| l.contains("/renew")).count();
        assert!(renews >= 5, "失敗之後要短間隔連續重試，不是每分鐘一次：只打了 {renews} 次");
    }

    /// 短暫的一次續約失敗、在到期之前下一次就成功：建置不能被誤殺；而且續約真的把租約往後延——
    /// 這一趟的虛擬時間走過好幾個 TTL，照樣跑完、退 0。
    #[test]
    fn one_failed_renew_before_expiry_does_not_kill_a_build_that_outlives_the_ttl() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_cargo_until_virtual(3 * 180 + 60);
        s.set_renew_mode("once_down");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["check"]);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "應該正常跑完");
        assert!(s.virtual_secs() > 3 * 180, "虛擬時間要走過好幾個 TTL 才算數：{}s", s.virtual_secs());
    }

    #[test]
    fn a_successful_renewal_without_a_send_timestamp_advances_the_conservative_deadline() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_cargo_until_virtual(3 * 180 + 60);
        s.fail_date_call(2); // acquire 時間可讀，第一次續約送出前的時間讀取失敗。
        s.set_renew_mode("success_then_fail_until_170");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["build"]);
        assert_eq!(rc, 0, "t=60 的續約已成功，daemon t=170 恢復時名額仍有效：{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "cargo 應該正常完成");
        assert!(s.virtual_secs() > 3 * 180, "虛擬時間要走過好幾個 TTL：{}s", s.virtual_secs());
    }

    /// 交替失敗（一次連不上、一次成功……）的租約是健康的：每次成功都把 deadline 往後延，所以永遠撐得到下一次成功。
    /// deadline 沒有隨續約成功往後延的話，第二次失敗就會照最早那個 deadline 判死。
    #[test]
    fn alternating_renew_failures_never_kill_a_lease_that_keeps_getting_renewed() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_cargo_until_virtual(3 * 180 + 60);
        s.set_renew_mode("alternate");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["test"]);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "應該正常跑完");
        assert!(s.virtual_secs() > 3 * 180, "{}s", s.virtual_secs());
    }

    /// issue #857：假 `sleep` 撥時鐘時旁邊一直有人在讀，shim 收尾時還會 SIGKILL 它——不管讀在哪一瞬間、殺在哪一瞬間，
    /// 時鐘都不能讀到空字串（`echo … > clock` 先截空再寫，舊版在這裡間歇讀到空的、或被殺後留下空檔）。
    #[test]
    fn the_virtual_clock_never_reads_empty_while_its_writer_is_killed_mid_tick() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let s = Sandbox::new();
        s.install_virtual_clock();
        std::fs::write(s.dir.join("cargo.ready"), "").unwrap();
        let clock = s.dir.join("clock");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let (clock, stop) = (clock.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut empty = 0;
                while !stop.load(Ordering::Relaxed) {
                    if std::fs::read_to_string(&clock).unwrap().trim().is_empty() {
                        empty += 1;
                    }
                }
                empty
            })
        };
        for i in 0..400u64 {
            let mut tick = Command::new(s.dir.join("real/sleep")).arg("1").spawn().unwrap();
            std::thread::sleep(Duration::from_micros(i % 13 * 150));
            let _ = tick.kill();
            tick.wait().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(reader.join().unwrap(), 0, "讀到空的時鐘");
        assert!(s.virtual_secs() >= 0, "時鐘被殺在半途就壞掉了");
    }

    /// issue #151：呼叫 shim 的那一端（agent 的 shell、被砍的測試行程）不在了，等名額的迴圈不能變成永遠在等的孤兒。
    /// 假的排程器永遠說額滿（`retry_after_secs=0`，等同忙等）；shim 的呼叫者是一個馬上結束的 `sh -c`。
    /// 以前這顆 shim 會一直轉下去，一輪全套就留下一批，多顆 agent 反覆跑就把機器的行程表塞滿。
    #[test]
    fn a_shim_whose_caller_is_gone_stops_waiting_for_a_slot() {
        let s = Sandbox::new();
        // 每一輪 acquire 記一行：判定「shim 有沒有發現呼叫端不在了」用**輪數**，不用牆鐘時間（整套平行跑時機器忙，同樣的一輪可以慢十倍）。
        s.install_fake_curl(&format!(
            r#"case "$*" in
  *acquire*) echo x >> '{}'; printf '{{"granted":false,"active":2,"retry_after_secs":0}}' ;;
  *) printf '{{}}' ;;
esac"#,
            s.dir.join("acquire.log").display()
        ));
        let env = lease_env(&s);
        // 外層是 `sh -c`（不是 shim 本身），所以不走 `s.command()`——但同樣清掉呼叫端所有的 `AM_*`。
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("'{}' build & echo $! > '{}'", s.dir.join("bin/cargo").display(), s.dir.join("shim.pid").display()))
            .env("PATH", format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()))
            .env("HOME", s.dir.join("fake-home"));
        for (key, _) in std::env::vars() {
            if key.starts_with("AM_") {
                cmd.env_remove(key);
            }
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // 外層 `sh -c` 起完 shim 就結束：shim 的 `$PPID` 指到一個已經不在的行程。
        let (mut outer, _, err_path) = s.start_group(&mut cmd, false);
        let group = outer.id() as i32;
        assert!(outer.wait().unwrap().success());
        assert_shim_gives_up_once_caller_is_gone(&s, group, &err_path);
    }

    /// 呼叫端不在之後，shim 最多再問 `MAX_ROUNDS_AFTER_GONE` 輪 acquire 就要自己講一句並退出。
    fn assert_shim_gives_up_once_caller_is_gone(s: &Sandbox, group: i32, err_path: &std::path::Path) {
        // 從這一刻起 shim 只要再問一輪 acquire 就該發現呼叫端不在了：
        // 判準是「之後又問了幾輪」（事件驅動、跟機器忙不忙無關），不是「等了幾秒」。牆鐘只留一個給真正卡死的後盾：
        // 這麼久**一輪都沒有進展**、也沒退出，才算卡住。
        let rounds = || std::fs::read_to_string(s.dir.join("acquire.log")).map(|t| t.lines().count()).unwrap_or(0);
        const MAX_ROUNDS_AFTER_GONE: usize = 25;
        let at_gone = rounds();
        let (mut last, mut moved) = (at_gone, std::time::Instant::now());
        let mut verdict = None;
        while !group_members(group).is_empty() {
            let n = rounds();
            if n - at_gone > MAX_ROUNDS_AFTER_GONE {
                verdict = Some(format!("呼叫端不在之後 shim 又問了 {} 輪 acquire 還沒退出", n - at_gone));
                break;
            }
            if n != last {
                (last, moved) = (n, std::time::Instant::now());
            } else if moved.elapsed() > std::time::Duration::from_secs(120) {
                verdict = Some(format!("shim 卡住了：120 秒一輪 acquire 都沒有（共 {} 輪）也沒退出", n - at_gone));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let left = group_members(group);
        s.kill_group_if_ours(group);
        let said = std::fs::read_to_string(err_path).unwrap_or_default();
        assert!(left.is_empty(), "呼叫端不在了，shim 還在等名額（{}）：{left:?}\n{said}", verdict.unwrap_or_default());
        // 光看「行程消失了」不夠（機器上可能有別的東西在清孤兒）：shim 要**自己**講一句並退出，才是它自己發現的。
        assert!(said.contains("呼叫端已經結束"), "shim 要自己發現呼叫端不在了，而不是被別人殺掉：{said}");
    }

    /// issue #676：在 systemd --user 底下（Linux 搬家後 daemon 與 pane 都在它底下），呼叫端走掉之後 shim 被
    /// **收養者** `systemd --user` 收養：它是 child subreaper、跟 shim 同一個使用者，`$PPID` 指到它、`kill -0` 會成功，
    /// shim 以前就把它當成呼叫端、永遠等名額（ubuntu-ci 在 systemd timer 底下穩定紅，ssh shell 底下綠）。
    /// 這裡不需要真的 systemd：外層換成一個設了 `PR_SET_CHILD_SUBREAPER`、`comm` 叫 `systemd` 的 python 行程，
    /// 它起完 `sh -c '<shim> & …'` 之後留著當收養者，直到 shim 走了才結束。
    #[cfg(target_os = "linux")]
    #[test]
    fn a_shim_adopted_by_a_systemd_subreaper_stops_waiting_for_a_slot() {
        let s = Sandbox::new();
        s.install_fake_curl(&format!(
            r#"case "$*" in
  *acquire*) echo x >> '{}'; printf '{{"granted":false,"active":2,"retry_after_secs":0}}' ;;
  *) printf '{{}}' ;;
esac"#,
            s.dir.join("acquire.log").display()
        ));
        let env = lease_env(&s);
        // 子 shell 先睡一下再 exec 成 shim：外層 sh 一定已經走了，shim 啟動時的 `$PPID` **必定**是收養者（不靠時序運氣）。
        let inner = format!("( sleep 0.3; exec '{}' build ) & echo $! > '{}'", s.dir.join("bin/cargo").display(), s.dir.join("shim.pid").display());
        let marker = s.dir.join("inner.done");
        // PR_SET_CHILD_SUBREAPER=36、PR_SET_NAME=15（comm 就是 ps -o comm= 看到的名字）。收養來的 shim 結束後由這裡收屍，
        // 沒有子行程了（ChildProcessError）就結束；最多撐 300 秒，測試失敗時也不會留下它。
        let py = r#"import ctypes, os, subprocess, sys, time
libc = ctypes.CDLL(None, use_errno=True)
assert libc.prctl(36, 1, 0, 0, 0) == 0, "PR_SET_CHILD_SUBREAPER"
libc.prctl(15, b"systemd", 0, 0, 0)
subprocess.run(["sh", "-c", sys.argv[1]], check=True)
open(sys.argv[2], "w").close()
end = time.time() + 300
while time.time() < end:
    try:
        os.waitpid(-1, os.WNOHANG)
    except ChildProcessError:
        break
    time.sleep(0.05)
"#;
        let mut cmd = Command::new("python3");
        cmd.arg("-c")
            .arg(py)
            .arg(inner)
            .arg(&marker)
            .env("PATH", format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()))
            .env("HOME", s.dir.join("fake-home"));
        for (key, _) in std::env::vars() {
            if key.starts_with("AM_") {
                cmd.env_remove(key);
            }
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let (outer, _, err_path) = s.start_group(&mut cmd, false);
        let group = outer.id() as i32;
        // 等外層的 `sh -c` 結束（shim 從這一刻起是孤兒、被那顆「systemd」收養）。
        let t0 = std::time::Instant::now();
        while !marker.exists() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(60), "外層的 sh -c 60 秒內沒結束");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_shim_gives_up_once_caller_is_gone(&s, group, &err_path);
    }

    /// issue #151（也是 #128 的延伸）：shim 自己被 `SIGKILL`（`trap` 沒機會跑、名額沒人放）時，背景的續約迴圈
    /// 不能永遠續下去把名額佔到重開機。cargo 還活著就繼續續約（它還在用容量，租約不能先掉）；
    /// cargo 也沒了，就把名額放掉、收掉狀態目錄、自己結束。
    #[test]
    fn a_renew_loop_whose_shim_was_killed_stops_once_cargo_is_gone_and_releases_the_slot() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        let env = lease_env(&s);
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.args(["check"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let (mut shim, _, _) = s.start_group(&mut cmd, false);
        let group = shim.id() as i32;
        let up = std::time::Instant::now();
        while !s.dir.join("cargo.ready").exists() {
            assert!(up.elapsed() < std::time::Duration::from_secs(30), "cargo 沒起來");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // shim 被 SIGKILL：續約迴圈與 cargo 都成了孤兒。
        unsafe { libc::kill(shim.id() as i32, libc::SIGKILL) };
        let _ = shim.wait();
        let renews = || std::fs::read_to_string(s.dir.join("curl.log")).unwrap().matches("/renew").count();
        let before = renews();
        // 等下一次續約這個**事件**（守衛一秒一秒睡，虛擬時鐘下一次續約要幾秒真時間）；30 秒只是放棄的上限。
        let up = std::time::Instant::now();
        while renews() <= before && up.elapsed() < std::time::Duration::from_secs(30) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(renews() > before, "cargo 還活著：續約要繼續（它還在用容量）");
        assert!(!group_members(group).is_empty(), "續約迴圈還在");

        // cargo 也沒了（連它前景那顆 `sleep 120` 一起收掉）：迴圈放掉名額、收掉狀態目錄、自己結束。
        for f in ["cargo.pid", "rustc.pid"] {
            if let Some(p) = s.pid(f) {
                s.kill_if_ours(p);
            }
        }
        for (pid, cmd) in group_members(group) {
            if cmd.contains("sleep 120") {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        s.assert_group_gone(group);
        let calls = std::fs::read_to_string(s.dir.join("curl.log")).unwrap();
        assert!(calls.contains("/release"), "名額要放掉：{calls}");
        let leftovers: Vec<_> = std::fs::read_dir(s.dir.join("tmp")).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "狀態目錄要清掉：{leftovers:?}");
    }

    /// issue #183：shim 被單獨 SIGKILL（守衛與 cargo 成了孤兒、ppid 變 1）**之後**租約才失效（daemon 收回名額）——名額已經不是
    /// 我們的，cargo 與它底下的編譯器照樣要停，不然別人拿到同一個名額也開始編，`max_concurrent` 被突破。以前 `am_kill_tree`
    /// 用「ppid 是不是 shim」確認目標，shim 死了之後永遠不成立，什麼都不停就 return。守衛停完還要自己放名額、收狀態目錄
    /// （shim 已經不在，沒有別人會做）。每一種 shell 都驗（含 macOS 的 bash 3.2）。
    #[test]
    fn a_lease_lost_after_the_shim_was_killed_still_stops_cargo_and_cleans_up() {
        std::thread::scope(|scope| {
            for sh in shells() {
                scope.spawn(move || {
                    let s = Sandbox::new();
                    s.install_virtual_clock();
                    s.install_lease_curl(180);
                    s.install_slow_cargo(120);
                    let mut cmd = s.command_in(Some(sh), &s.path());
                    cmd.args(["check"]);
                    for (k, v) in &lease_env(&s) {
                        cmd.env(k, v);
                    }
                    let (mut shim, _, _) = s.start_group(&mut cmd, false);
                    let group = shim.id() as i32;
                    s.wait_cargo_ready();
                    // shim 被 SIGKILL：守衛與 cargo 成了孤兒；租約這時還有效，cargo 照跑。
                    unsafe { libc::kill(shim.id() as i32, libc::SIGKILL) };
                    let _ = shim.wait();
                    assert!(s.alive("cargo.pid"), "{sh}: 租約還有效，cargo 不該停");
                    // daemon 現在收回名額：守衛下一次續約會被明確拒絕。
                    s.set_renew_mode("notfound");
                    let up = std::time::Instant::now();
                    while (s.alive("cargo.pid") || s.alive("rustc.pid")) && up.elapsed() < std::time::Duration::from_secs(20) {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    assert!(!s.alive("cargo.pid"), "{sh}: 租約失效了，孤兒 cargo 還在編（突破 max_concurrent）");
                    assert!(!s.alive("rustc.pid"), "{sh}: cargo 底下的編譯器還活著（孤兒）");
                    assert!(!s.dir.join("cargo.done").exists(), "{sh}: cargo 不能跑完");
                    // 守衛自己也要收工：整組沒有行程、名額放掉、狀態目錄清掉。
                    s.assert_group_gone(group);
                    let calls = std::fs::read_to_string(s.dir.join("curl.log")).unwrap();
                    assert!(calls.contains("/release"), "{sh}: 名額要放掉：{calls}");
                    assert!(s.lease_dirs().is_empty(), "{sh}: 狀態目錄要清掉：{:?}", s.lease_dirs());
                });
            }
        });
    }

    /// #183 的另一面：shim 死了之後靠「還在 shim 那一組」確認目標——**不在那一組**的行程（cargo 早已退出、pid 被回收重用給了別人）
    /// 租約失效時不能被殺。這裡把守衛認的 cargo pid 換成一個路人（自己的 process group）的 pid，模擬 pid 被重用。
    #[test]
    fn a_lease_lost_never_stops_a_process_outside_the_shims_process_group() {
        use std::os::unix::process::CommandExt as _;
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        let mut bystander = Command::new("/bin/sleep").arg("300").process_group(0).spawn().unwrap();
        let (mut shim, _group) = start_slow_shim(&s);
        let dirs = s.lease_dirs();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        std::fs::write(s.dir.join("tmp").join(&dirs[0]).join("pid"), bystander.id().to_string()).unwrap();
        unsafe { libc::kill(shim.id() as i32, libc::SIGKILL) };
        let _ = shim.wait();
        s.set_renew_mode("notfound");
        // 守衛判定失效、（不）動手之後會把狀態目錄收掉；等到它收完才能斷言路人還活著。
        let up = std::time::Instant::now();
        while !s.lease_dirs().is_empty() && up.elapsed() < std::time::Duration::from_secs(20) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let survived = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();
        assert!(s.lease_dirs().is_empty(), "守衛沒有在時限內收工");
        assert!(survived, "不在 shim 那一組的行程被租約失效誤殺了（pid 被重用時會殺到別人）");
    }

    /// issue #189：快速跑完的 cargo 讓 shim 的 `_release` 在續約守衛才剛起來時就送 TERM。守衛手上的 `sleep 60` 有兩個縫會被漏掉、成為孤兒
    /// （行程數是全機共用的資源，#151）：TERM 落在 `sleep &` 與記下它的 pid 之間，handler 不知道要殺誰；以及剛 fork 出來、還沒 exec 的 sleep
    /// 仍帶著守衛的 TERM handler，TERM 被那個 handler 吞掉、sleep 照睡。一次的機率很小（本機實測快跑 480 次留下 2～19 顆），
    /// 所以一口氣跑很多次——每一次 `run_group` 都會確認整個 process group 一顆行程都不剩。每一種 shell 都驗（含 macOS 的 bash 3.2）。
    #[test]
    fn a_guard_stopped_right_after_it_starts_never_leaves_its_sleep_behind() {
        std::thread::scope(|scope| {
            for sh in shells() {
                for _ in 0..4 {
                    scope.spawn(move || {
                        let s = Sandbox::new();
                        s.install_lease_curl(180); // renew 間隔 60 秒：守衛的 `sleep 60`
                        let env = lease_env(&s);
                        for i in 0..guard_iterations() {
                            // 預設的假 cargo 立刻退 0：租約才剛拿到、守衛才剛起來。
                            let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["check"]);
                            assert_eq!(rc, 0, "{sh} 第 {i} 次：{err}");
                        }
                    });
                }
            }
        });
    }

    /// 起一顆 shim（慢的假 cargo）放進自己的 process group，等 cargo 起來；回傳它與 group id。
    fn start_slow_shim(s: &Sandbox) -> (std::process::Child, i32) {
        let mut cmd = s.command(&s.path());
        cmd.args(["check"]);
        for (k, v) in &lease_env(s) {
            cmd.env(k, v);
        }
        let (shim, _, _) = s.start_group(&mut cmd, false);
        s.wait_cargo_ready();
        let group = shim.id() as i32;
        (shim, group)
    }

    /// 「下一次呼叫」：一個很快跑完的 shim（`AM_TEST_FAST` 讓假 cargo 立刻退 0），它啟動時要把沒人在用的租約狀態目錄收掉。
    fn run_the_next_shim(s: &Sandbox) {
        let mut env = lease_env(s);
        env.push(("AM_TEST_FAST", "1".into()));
        let (_, err, rc) = s.run(&as_refs(&env), &["check"]);
        assert_eq!(rc, 0, "{err}");
    }

    /// issue #154：pane 被關（整個 process group 被 SIGKILL）時，shim 的 `trap` 與續約守衛都沒機會清，
    /// `am-cargo-lease.*` 暫存目錄就一直留著。下一次 shim 啟動時要把「擁有者已不存在」的收掉。
    /// 先確認目錄真的被留下來（問題本身），再確認下一次呼叫之後不見了（連同它自己的也放乾淨）。
    #[test]
    fn a_lease_directory_left_by_a_killed_shim_is_reclaimed_by_the_next_run() {
        let s = Sandbox::new();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        let (mut shim, group) = start_slow_shim(&s);
        assert_eq!(s.lease_dirs().len(), 1, "跑到一半應該有一個租約狀態目錄：{:?}", s.lease_dirs());
        // 整個 pane 被關：shim、續約守衛、cargo 都沒機會收尾。
        s.kill_group(group);
        let _ = shim.wait();
        s.assert_group_gone(group);
        assert_eq!(s.lease_dirs().len(), 1, "被 SIGKILL 的 shim 沒機會清目錄（這就是要修的問題）：{:?}", s.lease_dirs());

        run_the_next_shim(&s);
        assert!(s.lease_dirs().is_empty(), "擁有者已不在的目錄要被下一次呼叫收掉：{:?}", s.lease_dirs());
    }

    /// 正在用的不能被清：A 還在編（shim、守衛、cargo 都活著），另一顆 shim 啟動並清了一輪，A 的目錄照舊；
    /// A 自己正常收尾時再自己清掉。
    #[test]
    fn a_lease_directory_in_use_is_left_alone_by_another_shims_sweep() {
        let s = Sandbox::new();
        s.install_lease_curl(180);
        s.install_slow_cargo(4);
        let (mut a, group) = start_slow_shim(&s);
        let mine = s.lease_dirs();
        assert_eq!(mine.len(), 1, "{mine:?}");

        run_the_next_shim(&s);
        assert_eq!(s.lease_dirs(), mine, "A 還在跑，別人不能把它的目錄清掉");

        assert!(a.wait().unwrap().success(), "A 正常跑完");
        s.assert_group_gone(group);
        assert!(s.lease_dirs().is_empty(), "A 自己收尾要清掉：{:?}", s.lease_dirs());
    }

    /// 擁有者「不完全死」的情況：只有 shim 被 SIGKILL，cargo 與續約守衛還活著（#151：它們會接手收尾）——cargo 還在用這個目錄，
    /// 不能被清；等整組都死了才算沒人在用、才收得掉。
    #[test]
    fn a_directory_is_in_use_while_cargo_outlives_the_shim() {
        let s = Sandbox::new();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        let (mut shim, group) = start_slow_shim(&s);
        let mine = s.lease_dirs();
        assert_eq!(mine.len(), 1, "{mine:?}");
        // 只殺 shim 本人：守衛（背景）與 cargo 成了孤兒、還在跑。
        unsafe { libc::kill(shim.id() as i32, libc::SIGKILL) };
        let _ = shim.wait();
        assert!(!group_members(group).is_empty(), "守衛與 cargo 還活著");

        run_the_next_shim(&s);
        assert_eq!(s.lease_dirs(), mine, "cargo 還在用這個目錄，不能清");

        // 整組都沒了：沒人在用了，下一次呼叫才收得掉。
        s.kill_group(group);
        s.assert_group_gone(group);
        run_the_next_shim(&s);
        assert!(s.lease_dirs().is_empty(), "{:?}", s.lease_dirs());
    }

    /// 清的範圍與判斷：只動自己名下、名字是 `am-cargo-lease.*` 的**目錄**（不是符號連結、不是檔案）；
    /// 名字裡帶 pid 的（現行格式）看那個 pid 死活；沒有 pid 的（舊版 shim 留下的）看裡面記的 pid，
    /// 再來要放夠久（一小時）才收——年輕的可能正在被建起來。
    #[test]
    fn the_sweep_only_takes_directories_nobody_owns_any_more() {
        let s = Sandbox::new();
        s.install_lease_curl(180);
        s.install_slow_cargo(1);
        let tmp = s.dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let mut dead = std::process::Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let dead_pid = dead.id();
        dead.wait().unwrap();
        let live_pid = std::process::id();
        let mkdir = |name: &str, pid_file: Option<u32>, old: bool| {
            let d = tmp.join(name);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(p) = pid_file {
                std::fs::write(d.join("pid"), p.to_string()).unwrap();
            }
            if old {
                assert!(std::process::Command::new("touch").args(["-t", "202001010000"]).arg(&d).status().unwrap().success());
            }
        };
        // 該收的：擁有者（名字裡的 pid）已死；舊格式、放很久、裡面記的 pid 也死了／沒有。
        mkdir(&format!("am-cargo-lease.{dead_pid}.aaaaaa"), None, false);
        mkdir("am-cargo-lease.OldDead", Some(dead_pid), true);
        mkdir("am-cargo-lease.OldNoPid", None, true);
        // 不該收的：擁有者還活著；舊格式但裡面記的 pid 還活著；舊格式很年輕（可能正在建）；不是目錄；符號連結；別的東西。
        mkdir(&format!("am-cargo-lease.{live_pid}.bbbbbb"), None, false);
        mkdir("am-cargo-lease.OldLive", Some(live_pid), true);
        mkdir("am-cargo-lease.Young1", None, false);
        std::fs::write(tmp.join("am-cargo-lease.afile"), "x").unwrap();
        let elsewhere = s.dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep"), "x").unwrap();
        // 名字是「擁有者已死」的格式：不是符號連結的話這個會被收掉，所以「還在」證明的是符號連結被放過。
        let link_name = format!("am-cargo-lease.{dead_pid}.cccccc");
        std::os::unix::fs::symlink(&elsewhere, tmp.join(&link_name)).unwrap();
        mkdir("unrelated-dir", None, true);

        run_the_next_shim(&s);
        let mut left: Vec<String> = std::fs::read_dir(&tmp).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mut want = vec![
            format!("am-cargo-lease.{live_pid}.bbbbbb"),
            "am-cargo-lease.OldLive".to_string(),
            "am-cargo-lease.Young1".to_string(),
            "am-cargo-lease.afile".to_string(),
            link_name,
            "unrelated-dir".to_string(),
        ];
        want.sort();
        assert_eq!(left, want, "只該收掉擁有者已不在的目錄");
        assert!(elsewhere.join("keep").exists(), "符號連結指到的地方不能動");
    }

    /// 前景執行的語意不能因為多了守衛而變：stdin 照樣接得到 cargo（`cargo run` 起來的程式要讀輸入）。
    #[test]
    fn the_guard_keeps_stdin_flowing_to_cargo() {
        let s = Sandbox::new();
        s.install_lease_curl(30);
        let sink = s.dir.join("stdin.got");
        write_exec(s.dir.join("real/cargo"), format!("#!/bin/sh\ncat > '{}'\nexit 0\n", sink.display()));
        let env = lease_env(&s);
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.args(["run"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let (_, err, rc) = s.run_group(cmd, Some(b"hello-from-the-terminal"));
        assert_eq!(rc, 0, "{err}");
        assert_eq!(std::fs::read_to_string(&sink).unwrap(), "hello-from-the-terminal");
    }

    /// issue #128 的整合驗收：**真的** router＋**真的** curl＋**真的** shim＋真時鐘（`max_concurrent=1`、TTL 21 秒：續約在 7、14 秒，
    /// 第二次失敗時離到期還有 7 秒、趕不及再試就停，餘裕夠大，機器忙也不會誤判）。
    /// A 拿到唯一的名額、開始「編譯」；daemon 中斷（server 整個關掉）超過 TTL。B 在同一段時間裡一直在問名額。
    /// 不變式：**任何時刻，活著的編譯器不超過設定的名額**——B 一拿到名額的那一刻，A 的 cargo 與它底下的
    /// 編譯器必須已經死了（以前續約失敗被吞掉、A 照跑，daemon 到期收回名額後 B 拿到、兩個同時編）。
    /// 之後 daemon 回來（新的 App 開同一個資料庫，就像重啟）：名額表還在、B 的持有正常、沒有卡死。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_outage_longer_than_the_lease_never_leaves_two_compilers_alive() {
        use crate::build_scheduler::{acquire, Acquired};
        use std::time::{Duration, Instant};

        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 1;
                c.build.lease_ttl_secs = 21;
                Ok(())
            })
            .await
            .unwrap();
        let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listener = bind();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let serve = |app: std::sync::Arc<crate::state::App>, listener: std::net::TcpListener| {
            let router = crate::api::router(app);
            tokio::spawn(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
            })
        };
        let server = serve(env.app.clone(), listener);

        // 沙盒：真的 shim、假 cargo（25 秒，會生一顆 300 秒的「rustc」）、真的 curl（PATH 上 /usr/bin/curl）。
        let s = Sandbox::new();
        s.install_slow_cargo(25);
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "test-token").unwrap();
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.env("AM_PORT", port.to_string())
            .env("TMPDIR", s.dir.join("tmp"))
            .args(["check", "-p", "agents-managerd"]);
        std::fs::create_dir_all(s.dir.join("tmp")).unwrap();
        let (mut a, _, shim_err) = s.start_group(&mut cmd, false);
        let a_group = a.id() as i32;

        // A 拿到名額、cargo 真的起來了。
        let up = Instant::now();
        while crate::build_scheduler::status(&env.app).await.unwrap()["active"] != 1 || s.pid("rustc.pid").is_none() {
            assert!(up.elapsed() < Duration::from_secs(30), "A 沒拿到名額：{}", std::fs::read_to_string(&shim_err).unwrap_or_default());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(s.alive("cargo.pid") && s.alive("rustc.pid"));

        // daemon 中斷（server 整個關掉）。B 從這一刻起每 100ms 問一次名額，直到拿到。
        let outage = Instant::now();
        server.abort();
        let mut a_dead_at: Option<Duration> = None;
        let granted_at = loop {
            assert!(outage.elapsed() < Duration::from_secs(60), "B 一直拿不到名額（名額表卡死了？）");
            if a_dead_at.is_none() && a.try_wait().unwrap().is_some() && !s.alive("cargo.pid") && !s.alive("rustc.pid") {
                a_dead_at = Some(outage.elapsed());
            }
            if let Acquired::Granted { .. } = acquire(&env.app, "B:1", None, "check", "local").await.unwrap() {
                break outage.elapsed();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let dead = a_dead_at.unwrap_or_else(|| panic!("B 拿到名額（{granted_at:?}）的時候，A 的 cargo／rustc 還活著——兩個編譯同時在跑，max_concurrent=1 被突破"));
        assert!(dead <= granted_at, "A 要先死（{dead:?}）B 才拿得到名額（{granted_at:?}）");
        assert_eq!(a.wait().unwrap().code(), Some(75), "被停掉的 A 退 75（可重試）：{}", std::fs::read_to_string(&shim_err).unwrap_or_default());
        assert!(!s.dir.join("cargo.done").exists(), "A 不能跑完");
        s.assert_group_gone(a_group);

        // daemon 回來（新的 App 開同一個資料庫）：B 持有的名額正常，名額表沒有卡死。
        let app2 = crate::testing::restart_app(&env).await;
        let server2 = serve(app2.clone(), bind_on(port));
        let st = crate::build_scheduler::status(&app2).await.unwrap();
        assert_eq!(st["active"], 1, "只有 B 一個：{st}");
        server2.abort();
    }

    /// 在指定的 port 上重開一個 listener（daemon 重啟後回到同一個位址）。
    fn bind_on(port: u16) -> std::net::TcpListener {
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        l.set_nonblocking(true).unwrap();
        l
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
        let path = format!(
            "{}:{}:{}:/usr/bin:/bin",
            s.dir.join("bin").display(),
            other.join("bin").display(),
            s.dir.join("real").display()
        );
        let mut cmd = s.command(&path);
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_PORT", "1").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        cmd.args(["check", "-p", "agents-managerd"]);
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        // 真 cargo 真的跑到了（不是卡在等名額），而且整趟只拿一個名額。
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "{err}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert_eq!(calls.matches("acquire").count(), 1, "一層一個名額就是死結：{calls}");
    }

    /// issue #731：daemon 用固定的最小 PATH 起（沒有 `~/.cargo/bin`），bot pane 繼承下來，PATH 上只剩 shim。
    /// 沒有 `AM_REAL_CARGO` 時要退回 `$CARGO_HOME/bin/cargo`，再退回 `$HOME/.cargo/bin/cargo`，不能 exit 127。
    #[test]
    fn with_no_real_cargo_on_path_the_shim_falls_back_to_the_rustup_home() {
        let s = Sandbox::new();
        let home_bin = s.dir.join("fake-home/.cargo/bin");
        std::fs::create_dir_all(&home_bin).unwrap();
        let log = s.dir.join("cargo.log");
        write_exec(home_bin.join("cargo"), format!("#!/bin/sh\necho home \"$@\" >> '{}'\n", log.display()));
        // PATH 上沒有 `real`（假 cargo 的目錄）：跟 bot pane 一樣，第一個 cargo 就是 shim 自己。
        let path = format!("{}:/usr/bin:/bin", s.dir.join("bin").display());
        let mut cmd = s.command(&path);
        cmd.env_remove("CARGO_HOME").arg("--version");
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "home --version\n", "{err}");

        // `$CARGO_HOME` 先於 `$HOME/.cargo`。
        let cargo_home = s.dir.join("cargo-home");
        std::fs::create_dir_all(cargo_home.join("bin")).unwrap();
        write_exec(cargo_home.join("bin/cargo"), format!("#!/bin/sh\necho cargo-home \"$@\" >> '{}'\n", log.display()));
        let mut cmd = s.command(&path);
        cmd.env("CARGO_HOME", &cargo_home).arg("--version");
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "home --version\ncargo-home --version\n", "{err}");
    }

    /// 同上，但 `$HOME/.cargo/bin` 也沒有 cargo：照舊 exit 127，訊息一字不改（使用者照它設 `AM_REAL_CARGO`）。
    #[test]
    fn with_no_real_cargo_anywhere_the_shim_still_exits_127() {
        let s = Sandbox::new();
        std::fs::create_dir_all(s.dir.join("fake-home/.cargo/bin")).unwrap();
        let path = format!("{}:/usr/bin:/bin", s.dir.join("bin").display());
        let mut cmd = s.command(&path);
        cmd.env_remove("CARGO_HOME").arg("--version");
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 127, "{err}");
        assert_eq!(err, "agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n");

        // 退回的位置一樣要過「是不是 shim」的防呆：`$CARGO_HOME/bin` 是另一顆 bot 的 shim 目錄時跳過，不能遞迴進去。
        let other = s.dir.join("bots/OTHER");
        super::install_local(&other).unwrap();
        let mut cmd = s.command(&path);
        cmd.env("CARGO_HOME", &other).arg("--version");
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 127, "{err}");
        assert_eq!(err, "agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n");
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
        write_exec(other.join("cargo"), &old_shim);
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
        let path = format!(
            "{}:{}:{}:/usr/bin:/bin",
            s.dir.join("bin").display(),
            other.display(),
            s.dir.join("real").display()
        );
        let mut cmd = s.command(&path);
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_PORT", "1").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        cmd.args(["build", "--release"]);
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
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
                ("AM_PORT", "1"),
                ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap()),
                ("AM_TEST_FAKE_CARGO_EXIT", "101"),
            ],
            &["build"],
        );
        assert_eq!(rc, 101);
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert!(calls.contains("release"), "失敗也要放：{calls}");
    }
