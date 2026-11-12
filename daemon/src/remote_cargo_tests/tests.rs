/// pre-1960 的歷史原樣 read_handshake 邏輯，專用於回歸對照測試。
fn pre_1960_read_handshake(r: &mut impl BufRead) -> anyhow::Result<Handshake> {
    let mut hs = Handshake::default();
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            anyhow::bail!("remote workdir guard exited before handing out a directory");
        }
        let f: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        match f.as_slice() {
            ["END"] if !hs.dir.is_empty() => return Ok(hs),
            ["ROOT", root] => hs.root = root.to_string(),
            ["DIR", dir, token] => {
                hs.dir = dir.to_string();
                hs.token = token.to_string();
            }
            ["D", path, k, b, idle] => hs.dirs.push(RemoteDir {
                path: path.to_string(),
                locked: *k == "1",
                busy: *b == "1",
                idle_secs: idle.parse::<i64>().unwrap_or(0).max(0) as u64,
            }),
            ["F", free, total] => hs.disk = free.parse().ok().zip(total.parse().ok()),
            ["SLOT", i, max] => hs.slot = i.parse().ok().zip(max.parse().ok()),
            ["BUSY", n] => return Err(anyhow::Error::new(Busy { waiters: n.parse().unwrap_or(0) })),
            _ => {}
        }
    }
}

#[test]
fn pre_1960_parser_equivalence_on_new_outcome_fixtures() {
    // 包含新 OUTCOME 結構化行的 fixture 輸入
    let out = "Welcome!\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t2\t3\nD\t/r/00000000000000aa/111\t0\t1\t-3\nD\t/r/00000000000000aa/222\t1\t0\t900\nF\t500\t1000\nOUTCOME\tshared\t153\t0\nEND\n";

    let old_res = pre_1960_read_handshake(&mut std::io::Cursor::new(out)).unwrap();
    let new_res = read_handshake(&mut std::io::Cursor::new(out)).unwrap();

    // 逐欄驗證舊 parser 與新 parser 回傳完全一致（新 OUTCOME 控制行被舊 parser 依 _ => {} 乾淨略過）
    assert_eq!(old_res.root, new_res.root);
    assert_eq!(old_res.dir, new_res.dir);
    assert_eq!(old_res.token, new_res.token);
    assert_eq!(old_res.dirs, new_res.dirs);
    assert_eq!(old_res.disk, new_res.disk);
    assert_eq!(old_res.slot, new_res.slot);
}

#[test]
fn read_handshake_backward_compatibility_without_outcome() {
    let out = "Welcome!\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t1\t3\nEND\n";
    let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
    let h = hs.unwrap();
    assert_eq!(h.dir, "/r/0123456789abcdef/shared");
    assert_eq!(obs.outcome, None);
    assert_eq!(obs.shared_wait_secs, None);
    assert_eq!(obs.slot_wait_secs, None);

    // 舊 wrapper 原樣成功
    let h_old = read_handshake(&mut std::io::Cursor::new(out)).unwrap();
    assert_eq!(h_old.dir, "/r/0123456789abcdef/shared");
}

#[test]
fn read_handshake_with_outcome_control_line() {
    let out = "ROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t2\t3\nOUTCOME\tshared\t153\t0\nEND\n";
    let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
    let h = hs.unwrap();
    assert_eq!(h.dir, "/r/0123456789abcdef/shared");
    assert_eq!(obs.outcome.as_deref(), Some("shared"));
    assert_eq!(obs.shared_wait_secs, Some(153));
    assert_eq!(obs.slot_wait_secs, Some(0));
}

#[test]
fn read_handshake_observed_captures_queue_full_outcome_before_eof() {
    let out = "OUTCOME\tqueue_full\t900\t4\n";
    let (hs, obs) = read_handshake_observed(&mut std::io::Cursor::new(out));
    assert!(hs.is_err(), "must fail due to EOF before DIR/END");
    assert_eq!(obs.outcome.as_deref(), Some("queue_full"));
    assert_eq!(obs.shared_wait_secs, Some(900));
    assert_eq!(obs.slot_wait_secs, Some(4));
}

#[test]
fn rsync_destination_brackets_ipv6_hosts() {
    let mut r = remote();
    r.host = "fd7a:115c:a1e0::5".into();
    assert_eq!(rsync_destination(&r, "/home/u/shared/"), "me@[fd7a:115c:a1e0::5]:/home/u/shared/");
    assert_eq!(rsync_destination(&remote(), "/home/u/shared/"), "me@build-host:/home/u/shared/");
}

/// #328、#1856、#1861、#1858：固定排除 git、根 target、根 `.claude`；`web/dist` 仍須同步，未明確排除時 node_modules 也是輸入。
#[test]
fn the_rsync_excludes_keep_source_dirs_named_target() {
    if !has_program(&rsync_program()) {
        return;
    }
    let base = crate::testing::track(std::env::temp_dir().join(format!("am-rsync-{}", std::process::id())));
    let (src, dst) = (base.join("src"), base.join("dst"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Cargo.toml"), "[workspace]\n").unwrap();
    for d in [
        "src/target",
        "crates/target",
        "target/debug",
        "web/node_modules",
        ".claude/worktrees/x/target",
        "crates/.claude",
    ] {
        std::fs::create_dir_all(src.join(d)).unwrap();
    }
    std::fs::write(src.join("src/target/mod.rs"), "x").unwrap();
    std::fs::write(src.join("crates/target/Cargo.toml"), "x").unwrap();
    std::fs::write(src.join("target/debug/big"), "x").unwrap();
    std::fs::write(src.join("web/node_modules/package.json"), "{}").unwrap();
    std::fs::create_dir_all(src.join("web/dist")).unwrap();
    std::fs::write(src.join("web/dist/index.html"), "CURRENT FRONTEND").unwrap();
    std::fs::write(src.join(".claude/worktrees/x/target/big"), "x").unwrap();
    std::fs::write(src.join("crates/.claude/keep.rs"), "x").unwrap();
    let excludes = rsync_exclusions(&src).unwrap();
    let manifest = scan_source_manifest(&src, &excludes).unwrap();
    assert!(manifest.entries.contains_key(b"web/dist".as_slice()), "frontend directory is a build input");
    assert!(manifest.entries.contains_key(b"web/dist/index.html".as_slice()), "frontend file is a build input");
    std::fs::create_dir_all(dst.join("web/dist")).unwrap();
    std::fs::write(dst.join("web/dist/index.html"), "STALE FRONTEND").unwrap();
    let mut cmd = Command::new(rsync_program());
    cmd.args(["-a", "--delete"]);
    for exclude in excludes {
        cmd.arg("--exclude").arg(exclude);
    }
    let st = cmd
        .arg(format!("{}/", src.display()))
        .arg(format!("{}/", dst.display()))
        .output()
        .unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    assert!(dst.join("src/target/mod.rs").exists(), "src/target 是原始碼，要送");
    assert!(dst.join("crates/target/Cargo.toml").exists(), "crates/target 是 workspace 成員，要送");
    assert!(!dst.join("target").exists(), "根目錄的 target 不送");
    assert!(!dst.join(".claude").exists(), "根目錄的 .claude（含 worktree 的 target）不送");
    assert_eq!(std::fs::read_to_string(dst.join("web/dist/index.html")).unwrap(), "CURRENT FRONTEND", "rsync replaces stale frontend content for every profile");
    assert!(dst.join("crates/.claude/keep.rs").is_file(), "巢狀的 .claude 是原始碼，要送");
    assert!(
        dst.join("web/node_modules/package.json").is_file(),
        "未設定排除政策時，保留 node_modules 輸入"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// #329：手改 config 的極大 timeout_secs 不能讓 arm() 溢位 panic；單次 ssh 卡住要有上限。
#[test]
fn a_huge_timeout_does_not_panic_and_a_hung_ssh_is_bounded() {
    let d = Deadline::new(u64::MAX);
    d.arm();
    assert!(d.stop().is_none());
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let t = std::time::Instant::now();
    let err = output_with_timeout(cmd, std::time::Duration::from_millis(300)).unwrap_err().to_string();
    assert!(err.contains("沒有結束") && t.elapsed() < std::time::Duration::from_secs(25), "{err}");
}

/// #417：shim 在 `AM_DATA_DIR` 被清掉時（`scripts/check.sh`）不帶 `--data-dir`，helper 要照 daemon 的規則從設定檔推：
/// 密碼檔在資料目錄裡，推錯就是認證失敗。給了就照用。
#[test]
fn without_a_data_dir_the_helper_resolves_it_from_the_config_like_the_daemon() {
    let base = crate::testing::track(std::env::temp_dir().join(format!("am-r417-{}", crate::db::ulid())));
    let (beside, custom) = (base.join("conf"), base.join("conf/data"));
    std::fs::create_dir_all(&custom).unwrap();
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
    let cfg = beside.join("config.toml");
    std::fs::write(&cfg, "[build.remote]\nenabled = true\n").unwrap();
    assert_eq!(canon(&resolve_data_dir(&cfg, None).unwrap()), canon(&beside), "沒寫 [server] data_dir：設定檔所在目錄");
    assert_eq!(canon(&resolve_data_dir(&cfg, Some(Path::new(""))).unwrap()), canon(&beside), "空字串當沒給");
    std::fs::write(&cfg, "[server]\ndata_dir = \"data\"\n").unwrap();
    assert_eq!(canon(&resolve_data_dir(&cfg, None).unwrap()), canon(&custom), "[server] data_dir 優先");
    assert_eq!(resolve_data_dir(&cfg, Some(&base)).unwrap(), base, "給了就照用");
    std::fs::remove_dir_all(&base).ok();
}

/// #324／#325：設定檔壞掉、host 空、或本機設了 RUSTFLAGS 這類遠端看不到的變數——要退回本機也要講出原因，不能靜默；
/// 正常啟用且沒有這些變數才轉遠端；檔案不存在／沒啟用是本來就不轉，不必吵。
#[test]
fn the_offload_decision_never_falls_back_silently_when_something_is_wrong() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-decide-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    let args = vec!["test".to_string()];
    let no_env = || Vec::<(String, String)>::new();
    let write = |body: &str| {
        let p = dir.join("c.toml");
        std::fs::write(&p, body).unwrap();
        p
    };
    let good = "[build.remote]\nenabled = true\nhost = \"h\"\nuser = \"u\"\n";
    assert!(matches!(decide_offload(&write(good), &args, no_env()), Offload::Go(_)));
    assert!(matches!(decide_offload(&dir.join("missing.toml"), &args, no_env()), Offload::Local(None)), "沒設定檔＝沒啟用，不吵");
    assert!(matches!(decide_offload(&write("[build.remote]\nenabled = false\n"), &args, no_env()), Offload::Local(None)));
    assert!(matches!(decide_offload(&write("[[["), &args, no_env()), Offload::Local(Some(_))), "壞掉的設定檔要講");
    assert!(matches!(decide_offload(&write("[build.remote]\nenabled = true\n"), &args, no_env()), Offload::Local(Some(_))), "啟用卻沒 host／user 要講");
    let env = vec![("RUSTFLAGS".to_string(), "-D warnings".to_string())];
    match decide_offload(&write(good), &args, env) {
        Offload::Local(Some(why)) => assert!(why.contains("RUSTFLAGS"), "{why}"),
        _ => panic!("RUSTFLAGS 有設就不能轉遠端（遠端看不到，會假綠）"),
    }
    assert!(env_blocking_offload([("RUSTFLAGS".to_string(), String::new())]).is_none(), "空值不算");
    assert!(env_blocking_offload([("PATH".to_string(), "/x".to_string())]).is_none());
    for key in ["CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
        match decide_offload(&write(good), &args, [(key.to_string(), "/outside-cache".to_string())]) {
            Offload::Local(Some(why)) => assert!(why.contains(key), "{why}"),
            _ => panic!("{key} must keep the invocation local"),
        }
        assert!(matches!(decide_offload(&write(good), &args, [(key.to_string(), String::new())]), Offload::Go(_)), "empty {key} must allow offloading");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// config.toml 是人（或有寫檔權限的 agent）手改得到的，不只有 API 寫得進來：API 擋了的 host／user／remote_root 在這裡也要擋，
/// 不然 `host = "-oProxyCommand=…"`、`user = "-o…"` 會變成 ssh 的選項、`remote_root` 的 `$(…)` 會被 rsync 的遠端 shell 展開。
/// 擋下來就退回本機並講原因（跟設定檔壞掉同一條路）。
#[test]
fn a_hand_edited_config_with_option_or_shell_metacharacters_never_reaches_ssh() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-decide-bad-{}", crate::db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let args = vec!["test".to_string()];
    let decide = |host: &str, user: &str, root: &str, port: u32| {
        let p = dir.join("c.toml");
        let body = format!("[build.remote]\nenabled = true\nhost = {host:?}\nuser = {user:?}\nremote_root = {root:?}\nssh_port = {port}\n");
        std::fs::write(&p, body).unwrap();
        decide_offload(&p, &args, Vec::<(String, String)>::new())
    };
    assert!(matches!(decide("h.example", "u", "/srv/cargo", 22), Offload::Go(_)), "正常的照轉");
    assert!(matches!(decide("2001:db8::1", "me.x-y_z", ".cache/a/b", 2222), Offload::Go(_)), "IPv6、相對路徑也正常");
    for (host, user, root, port) in [
        ("-oProxyCommand=touch /tmp/pwn", "u", "/r", 22),
        ("h;id", "u", "/r", 22),
        ("h h", "u", "/r", 22),
        ("h\nid", "u", "/r", 22),
        ("h", "-oProxyCommand=touch /tmp/pwn", "/r", 22),
        ("h", "u;id", "/r", 22),
        ("h", "u", "/a/../b", 22),
        ("h", "u", "/a b", 22),
        ("h", "u", "$(touch /tmp/pwn)", 22),
        ("h", "u", "/a'b", 22),
        ("h", "u", "/a`id`", 22),
        ("h", "u", "-rf", 22),
    ] {
        match decide(host, user, root, port) {
            Offload::Local(Some(why)) => assert!(why.contains("不合法") || why.contains("不支援"), "{host:?} {user:?} {root:?}：{why}"),
            _ => panic!("{host:?} {user:?} {root:?} 不能轉到 ssh"),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// `sh_quote("")` 以前回空字串：`cargo test -- ''`（空的過濾字串）送到遠端少一個引數，跟本機不是同一個意思。
#[test]
fn an_empty_argument_survives_quoting() {
    assert_eq!(sh_quote(""), "''");
    let s = run_script("/r/0123456789abcdef/shared", "", &LeaseToken::new(), 1, 0, &["test".into(), "--".into(), String::new(), "a b".into()]);
    assert!(s.ends_with(" test -- '' 'a b'"), "{s}");
}

use super::*;

fn remote() -> BuildRemoteCfg {
    BuildRemoteCfg { host: "build-host".into(), user: "me".into(), ssh_port: 2222, ..Default::default() }
}

fn envs(cmd: &Command) -> std::collections::HashMap<String, String> {
    cmd.get_envs()
        .filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned())))
        .collect()
}

/// 沒有 `sshpass` 也要連得上：改用 ssh 自己的 askpass（OpenSSH 8.4+），密碼只走環境變數。
/// 2026-09-18：這台機器沒有 sshpass（homebrew core 沒有這個 formula），一按「測試」就被擋。
#[test]
fn without_sshpass_the_password_goes_through_ssh_askpass() {
    let helper = PathBuf::from("/d/remote-cargo-askpass.sh");
    let cmd = ssh_cmd(&remote(), Some("hunter2"), Some(&PwMode::Askpass(helper.clone())));
    assert_eq!(cmd.get_program(), "ssh");
    let e = envs(&cmd);
    assert_eq!(e.get("SSH_ASKPASS").map(String::as_str), Some("/d/remote-cargo-askpass.sh"));
    assert_eq!(e.get("SSH_ASKPASS_REQUIRE").map(String::as_str), Some("force"));
    assert_eq!(e.get("AM_SSH_PASSWORD").map(String::as_str), Some("hunter2"));
    let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert!(args.iter().any(|a| a == "NumberOfPasswordPrompts=1"), "密碼錯要當場失敗，不能卡在提示上：{args:?}");
    // 密碼不進 argv——`ps` 看得到別人的 argv。
    assert!(!args.iter().any(|a| a.contains("hunter2")), "{args:?}");

    // 有 sshpass 的機器照舊。
    let cmd = ssh_cmd(&remote(), Some("hunter2"), Some(&PwMode::Sshpass));
    assert_eq!(cmd.get_program(), "sshpass");
    assert_eq!(envs(&cmd).get("SSHPASS").map(String::as_str), Some("hunter2"));

    // 沒有密碼的一律不碰這些環境變數。
    let cmd = ssh_cmd(&remote(), None, None);
    assert_eq!(cmd.get_program(), "ssh");
    assert!(envs(&cmd).is_empty(), "{:?}", envs(&cmd));
}

/// #177：`cd daemon && cargo check` 到遠端時，要同步的是**工作區根**（有根的 Cargo.lock／[profile]／.cargo/config.toml），
/// 再在同一個子目錄下跑。以前一律同步 cwd 本身：遠端沒有鎖檔、重新解析依賴，驗的不是本機那份。
#[test]
fn a_subdirectory_call_syncs_the_workspace_root_and_runs_in_the_same_subdirectory() {
    // macOS 的 TMPDIR 在 /var/folders/…（/var → /private/var）。source_root 回的是 canonical 路徑，
    // 所以測試的基底也要先 canonicalize，兩邊才是同一種寫法；否則就只有 macOS 紅。
    let t = crate::testing::track(std::env::temp_dir().canonicalize().unwrap().join(format!("am-r177-{}", crate::db::ulid())));
    let mk = |rel: &str, manifest: Option<&str>| {
        let d = t.join(rel);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(m) = manifest {
            std::fs::write(d.join("Cargo.toml"), m).unwrap();
        }
        d
    };
    let pkg = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n";
    let ws = mk("ws", Some("[workspace]\nmembers = [\"daemon\"]\n"));
    let daemon = mk("ws/daemon", Some(pkg));
    let src = mk("ws/daemon/src", None);
    let solo = mk("solo", Some(pkg));
    let solo_src = mk("solo/src", None);
    let bare = mk("bare/x", None);
    // 自己就是另一個工作區根的目錄（不歸外層管）：它自己當根。
    let inner = mk("ws/tools", Some("[workspace]\n"));
    let inner_src = mk("ws/tools/src", None);
    // manifest 壞掉的目錄不算工作區根，也不能讓 helper 出事。
    let broken = mk("broken", Some("[workspace\n"));

    assert_eq!(source_root(&ws), (ws.clone(), String::new()), "就在根：不變");
    assert_eq!(source_root(&daemon), (ws.clone(), "daemon".into()), "成員目錄：往上找到工作區根");
    assert_eq!(source_root(&src), (ws.clone(), "daemon/src".into()), "更深的子目錄也一樣");
    assert_eq!(source_root(&solo), (solo.clone(), String::new()));
    assert_eq!(source_root(&solo_src), (solo.clone(), "src".into()), "沒有工作區的單一套件：套件根");
    assert_eq!(source_root(&bare), (bare.clone(), String::new()), "不在 cargo 專案裡：cwd 本身，跟以前一樣");
    assert_eq!(source_root(&inner_src), (inner.clone(), "src".into()), "有自己 [workspace] 的算自己的根");
    assert_eq!(source_root(&broken), (broken.clone(), String::new()), "壞 manifest：當成一般套件根");
    #[cfg(unix)]
    {
        let member_link = t.join("member-link");
        std::os::unix::fs::symlink(&daemon, &member_link).unwrap();
        assert_eq!(source_root(&member_link), (ws.clone(), "daemon".into()), "symlink 成員也必須同步 workspace 根");
    }
    let _ = std::fs::remove_dir_all(t);
}

/// #174：連線靜默斷掉（Wi-Fi 換 AP、Mac 睡著醒來 IP 變了、遠端掉電）時，ssh 沒有 ServerAlive 就只能等 TCP keepalive
/// （預設 2 小時）——helper 不返回、agent 的 cargo 卡死，遠端的鎖與目錄也一直被佔。沒有 ConnectTimeout，連上就不說話的對端也
/// 讓 ssh 永遠停在握手。ssh 那條（守門／run／probe／install）與 rsync 的 `-e` 都要帶，有沒有密碼都一樣。
#[test]
fn every_ssh_of_the_offload_gives_up_on_a_silent_peer() {
    fn has(args: &[String], opt: &str) -> bool {
        args.windows(2).any(|w| w[0] == "-o" && w[1].starts_with(opt))
    }
    let ssh_args = |password: Option<&str>, mode: Option<&PwMode>| -> Vec<String> {
        ssh_cmd(&remote(), password, mode).get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    };
    let variants = [
        ssh_args(None, None),
        ssh_args(Some("pw"), Some(&PwMode::Sshpass)),
        ssh_args(Some("pw"), Some(&PwMode::Askpass(PathBuf::from("/d/askpass.sh")))),
    ];
    for args in &variants {
        for opt in ["ConnectTimeout=", "ServerAliveInterval=", "ServerAliveCountMax="] {
            assert!(has(args, opt), "ssh 少了 {opt}：{args:?}");
        }
    }
    for (password, askpass) in [(false, false), (true, false), (true, true)] {
        let rsh = rsync_rsh(&remote(), password, askpass);
        for opt in ["ConnectTimeout=", "ServerAliveInterval=", "ServerAliveCountMax="] {
            assert!(rsh.contains(&format!("-o {opt}")), "rsync 的 -e 少了 {opt}：{rsh}");
        }
        assert!(rsh.starts_with("ssh -p 2222 "), "{rsh}");
    }
}

/// issue #104：密碼留空＝key／agent。沒有 sshpass／askpass、BatchMode=yes；有 `identity_file` 時 ssh 與 rsync 都帶同一把 `-i`；找不到檔案要明確報錯。
#[test]
fn a_blank_password_uses_key_auth_and_identity_file_reaches_ssh_and_rsync() {
    let mut r = remote();
    r.identity_file = "/keys/my key".into();
    let cmd = ssh_cmd(&r, None, None);
    let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(cmd.get_program(), "ssh");
    assert!(envs(&cmd).is_empty(), "空密碼不該有 SSHPASS／askpass：{:?}", envs(&cmd));
    assert!(args.contains(&"BatchMode=yes".to_string()), "{args:?}");
    let i = args.iter().position(|a| a == "-i").expect("少了 -i");
    assert_eq!(args[i + 1], "/keys/my key");
    assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
    let rsh = rsync_rsh(&r, false, false);
    assert!(rsh.contains("BatchMode=yes") && rsh.contains("-i '/keys/my key' -o IdentitiesOnly=yes"), "{rsh}");
    assert!(!rsync_rsh(&remote(), false, false).contains("-i "), "沒設就不能帶 -i");
    let err = check_identity(&r).unwrap_err().to_string();
    assert!(err.contains("identity_file") && err.contains("/keys/my key"), "{err}");
    assert!(check_identity(&remote()).is_ok());
}

/// issue #194：整體上限預設 12 分鐘、可設定；舊的 config.toml 沒寫這個 key 也是 12 分鐘；`0`＝不設上限。
#[test]
fn the_overall_limit_defaults_to_twelve_minutes_and_is_configurable() {
    assert_eq!(BuildRemoteCfg::default().timeout_secs, 720);
    let old: BuildRemoteCfg = toml::from_str("enabled = true\nhost = \"h\"\nuser = \"u\"\n").unwrap();
    assert_eq!(old.timeout_secs, 720, "舊設定沒有這個 key：用預設");
    let set: BuildRemoteCfg = toml::from_str("timeout_secs = 90\n").unwrap();
    assert_eq!(set.timeout_secs, 90);
    assert_eq!(limit_text(720), "12 分鐘");
    assert_eq!(limit_text(90), "90 秒");
    let unlimited = Deadline::new(0);
    unlimited.arm();
    assert!(unlimited.stop().is_none(), "0＝不設上限");
    // issue #104：上限從拿到名額與目錄（arm）才開始算，排隊的時間不算。
    let d = Deadline::new(1);
    assert!(d.limit.is_some() && d.at.get().is_none(), "還沒 arm：不計時");
    d.arm();
    assert!(d.at.get().is_some());
}

/// 子行程已經成功結束、但它留下的孫行程還握著 stderr pipe（ssh 的背景 master、ProxyCommand）：收尾不能等到孫行程自己退出。
#[test]
fn a_grandchild_holding_stderr_does_not_stall_a_finished_child() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo oops >&2; sleep 12 & exit 0");
        let d = Deadline::new(0);
        d.arm();
        let _ = tx.send(run_status_tee_stderr(cmd, "t", &d));
    });
    let (status, seen) = rx
        .recv_timeout(std::time::Duration::from_secs(8))
        .expect("孫行程握著 pipe 時，已結束的子行程不能卡住收尾")
        .unwrap();
    assert!(status.success());
    assert!(seen.contains("oops"), "已經讀到的 stderr 要留下來：{seen:?}");
}

/// `ssh -V` 的版本要照數字比：字串比會說 `10.3` 比 `8.4` 小。
#[test]
fn the_openssh_version_gate_compares_numbers_not_strings() {
    assert!(openssh_at_least_8_4("10.3p1, LibreSSL 3.3.6"));
    assert!(openssh_at_least_8_4("9.0p1"));
    assert!(openssh_at_least_8_4("8.4p1"));
    assert!(!openssh_at_least_8_4("8.3p1"));
    assert!(!openssh_at_least_8_4("7.9p1"));
    assert!(!openssh_at_least_8_4("not a version"));
}

/// askpass 腳本要是 0700、內容正確，而且重複呼叫不會一直重寫。
#[test]
fn the_askpass_helper_is_written_once_and_is_not_world_readable() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-askpass-{}", crate::db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let path = askpass_helper(&dir).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.starts_with("#!/bin/sh"), "{body}");
    assert!(body.contains("$AM_SSH_PASSWORD"), "{body}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(askpass_helper(&dir).unwrap(), path);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before, "內容沒變就不要重寫");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 連得上但沒有 cargo：這不是錯誤，是「還沒裝工具鏈」，UI 要據此提示幫忙安裝。
#[test]
fn a_probe_without_cargo_is_reported_as_a_missing_toolchain() {
    let (os, arch, cargo, ver) = parse_probe("OS=Linux\nARCH=x86_64\nCARGO=\n");
    assert_eq!(os.as_deref(), Some("Linux"));
    assert_eq!(arch.as_deref(), Some("x86_64"));
    assert!(cargo.is_none(), "{cargo:?}");
    assert!(ver.is_none());

    let (_, _, cargo, ver) = parse_probe("OS=Linux\nARCH=x86_64\nCARGO=/home/user/.cargo/bin/cargo\ncargo 1.90.0 (abc 2026-08-01)\n");
    assert_eq!(cargo.as_deref(), Some("/home/user/.cargo/bin/cargo"));
    assert_eq!(ver.as_deref(), Some("cargo 1.90.0 (abc 2026-08-01)"));
}

/// rustup 只改 shell profile，而 ssh 的非互動 shell 不讀 profile——probe、安裝後的檢查與真正
/// 的遠端 cargo 都要自己把 `~/.cargo/bin` 接到 PATH 前面，否則裝好了照樣說「沒有 cargo」。
#[test]
fn every_remote_command_puts_cargo_bin_on_the_path() {
    assert!(PROBE_SH.contains("$HOME/.cargo/bin:$PATH"), "{PROBE_SH}");
    assert!(INSTALL_SH.contains("$HOME/.cargo/bin:$PATH"), "{INSTALL_SH}");
    // 安裝是冪等的：已經有就只回版本，不會再跑一次 rustup-init。
    assert!(INSTALL_SH.contains("if command -v cargo >/dev/null 2>&1; then\nprintf 'ALREADY='"), "{INSTALL_SH}");
    // minimal（另外帶 clippy，issue #104），而且不去動使用者的 shell profile。
    assert!(INSTALL_SH.contains("--profile minimal --component clippy"), "{INSTALL_SH}");
    assert!(INSTALL_SH.contains("--no-modify-path"), "{INSTALL_SH}");
    // rustup 不裝 linker：沒有 cc 的機器要當場講，不要等到 cargo test 連結失敗才看到天書。
    assert!(INSTALL_SH.contains("CC_MISSING=1"), "{INSTALL_SH}");
}

/// #670：登入 shell 是 fish／tcsh 時，裸的 `VAR=value` 腳本直接被拒。外層只接受 `sh -c '…'`。
#[test]
fn macos_local_remote_command_is_parsed_by_sh_not_the_login_shell() {
    let fish = r#"cmd=$1
case "$cmd" in
  "sh -c "*) ;;
  *) echo fish-reject >&2; exit 3 ;;
esac
inner=$(python3 -c 'import shlex,sys; print(shlex.split(sys.argv[1])[2])' "$cmd")
exec /bin/sh -c "$inner"
"#;
    let run = |remote_arg: &str| {
        let out = Command::new("/bin/sh").arg("-c").arg(fish).arg("fish").arg(remote_arg).output().unwrap();
        (out.status.code().unwrap_or(99), String::from_utf8_lossy(&out.stderr).into_owned(), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let raw = run(PROBE_SH);
    assert_eq!(raw.0, 3, "沒包 sh -c 時 fish 要拒：{raw:?}");
    let wrapped = run(&login_shell_arg(PROBE_SH));
    assert_ne!(wrapped.0, 3, "包了 sh -c 不該被 fish 拒：{wrapped:?}");
    assert!(wrapped.2.contains("OS="), "{wrapped:?}");
    for script in [INSTALL_SH, &run_script("/r/job", "", &LeaseToken::new(), 1, 0, &["--version".into()])] {
        let (code, err, _) = run(&login_shell_arg(script));
        assert_ne!(code, 3, "fish 拒了：{err} {script}");
    }
}

/// 在 `sh` 裡真的跑遠端的腳本：`HOME` 是空沙盒、PATH 只有 `uname`，`cargo`／`rustup` 用 shell 函式假裝（不寫可執行檔，免得撞 ETXTBSY）。
fn run_remote_sh(script: &str, fakes: &str) -> String {
    let sandbox = crate::testing::track(std::env::temp_dir().join(format!("am-probe-{}", crate::db::ulid())));
    std::fs::create_dir_all(sandbox.join("bin")).unwrap();
    let uname = String::from_utf8(Command::new("sh").arg("-c").arg("command -v uname").output().unwrap().stdout).unwrap();
    std::os::unix::fs::symlink(uname.trim(), sandbox.join("bin/uname")).unwrap();
    // PATH 換掉之後 `sh` 也照新的 PATH 找：用絕對路徑。
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{fakes}\n{script}"))
        .env("HOME", &sandbox)
        .env("PATH", sandbox.join("bin"))
        .env("AM_TEST_SANDBOX", &sandbox)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&sandbox);
    String::from_utf8(out.stdout).unwrap()
}

/// issue #104：probe 要分得出「沒有 cargo」「有 cargo 沒 clippy」「都有」。clippy 是轉過去的三個指令之一，
/// 有 cargo 沒 clippy 時要報 `clippy_missing`，不能說「可用」然後第一次 `cargo clippy` 才失敗。沒有 cargo 時 `CARGO=` 不能跟下一行黏在一起。
#[test]
fn the_probe_tells_a_missing_clippy_apart_from_a_missing_cargo() {
    let none = run_remote_sh(PROBE_SH, "");
    assert!(none.contains("\nCARGO=\n"), "{none}");
    let (_, _, cargo, _) = parse_probe(&none);
    assert_eq!((cargo, probe_clippy(&none)), (None, None), "{none}");

    let no_clippy = run_remote_sh(PROBE_SH, "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; *) return 1 ;; esac; }");
    let (_, _, cargo, version) = parse_probe(&no_clippy);
    assert_eq!((cargo.as_deref(), version.as_deref(), probe_clippy(&no_clippy)), (Some("cargo"), Some("cargo 1.90.0"), None), "{no_clippy}");

    let both = run_remote_sh(
        PROBE_SH,
        "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; clippy) echo 'clippy 0.1.90 (abc 2026-08-01)' ;; esac; }",
    );
    assert_eq!(probe_clippy(&both).as_deref(), Some("clippy 0.1.90 (abc 2026-08-01)"), "{both}");
}

/// issue #104：本來就有 cargo（只是 minimal、沒有 clippy）的機器按「安裝」要補上 clippy；補不上（不是 rustup 裝的）就回報 `CLIPPY_MISSING=1`。
#[test]
fn installing_on_a_host_that_already_has_cargo_adds_clippy() {
    // `rustup component add clippy` 會真的讓 `cargo clippy` 變成可用。
    let cargo = "cargo() { case \"$1\" in --version) echo 'cargo 1.90.0' ;; clippy) [ -e \"$AM_TEST_SANDBOX/clippy\" ] && echo 'clippy 0.1.90' ;; esac; }";
    let rustup = "rustup() { [ \"$*\" = 'component add clippy' ] && : > \"$AM_TEST_SANDBOX/clippy\"; }";
    let out = run_remote_sh(INSTALL_SH, &format!("{cargo}\n{rustup}"));
    assert!(out.contains("ALREADY=cargo 1.90.0"), "{out}");
    assert!(!out.contains("CLIPPY_MISSING"), "rustup 補上了 clippy：{out}");

    let out = run_remote_sh(INSTALL_SH, cargo);
    assert!(out.contains("CLIPPY_MISSING=1"), "沒有 rustup 補不上：要講出來：{out}");
}

#[test]
fn only_cross_platform_verification_commands_are_offloaded() {
    for cmd in ["c", "check", "t", "test", "clippy"] {
        assert!(eligible(&[cmd.into()]), "{cmd}");
    }
    for cmd in ["build", "run", "bench", "doc", "install", "metadata"] {
        assert!(!eligible(&[cmd.into()]), "{cmd}");
    }
    // issue #195：子指令前面的全域旗標。搬得走的（原樣搬過去意思不變）照樣轉；`+toolchain`／本機路徑／取決於本機快取的留在本機。
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for ok in [&["--locked", "test"][..], &["-q", "check"], &["--color", "always", "clippy"], &["--color=never", "t"], &["-v", "--locked", "test", "--", "x"]] {
        assert!(eligible(&v(ok)), "{ok:?}");
    }
    for no in [&["+nightly", "test"][..], &["-C", "x", "test"], &["--offline", "test"], &["--frozen", "check"], &["-Z", "unstable-options", "check"], &["--config", "a=b", "test"], &["--locked", "build"], &["--locked"], &[]] {
        assert!(!eligible(&v(no)), "{no:?}");
    }
    // issue #104：`clippy --fix` 會改原始碼——改到的是遠端那份、不會同步回來，所以留在本機跑。`--` 後面是給 clippy-driver 的，不算。
    for local in [&["clippy", "--fix"][..], &["clippy", "--all-targets", "--fix", "--allow-dirty"], &["-q", "clippy", "--fix", "--", "-D", "warnings"]] {
        assert!(!eligible(&v(local)), "{local:?}");
    }
    for remote in [&["clippy", "--all-targets", "-p", "x"][..], &["clippy", "--", "-D", "warnings"], &["clippy", "--", "--fix"]] {
        assert!(eligible(&v(remote)), "{remote:?}");
    }
    for local in [
        &["check", "--target-dir", "/other/cache"][..],
        &["test", "--target-dir=/other/cache"],
        &["clippy", "--config", "build.target-dir='elsewhere'"],
        &["check", "--config=build.build-dir='elsewhere'"],
    ] {
        assert!(!eligible(&v(local)), "custom cache directories must remain local: {local:?}");
    }
    assert!(eligible(&v(&["test", "--", "--target-dir=filter"])));
    for remote in [
        &["check", "--release"][..],
        &["test", "--profile", "release"],
        &["clippy", "--profile=release"],
    ] {
        assert!(eligible(&v(remote)), "release checks are safe because all source inputs are synced: {remote:?}");
    }
}

#[test]
fn blaze_accepts_only_safe_test_check_and_fails_closed_before_remote_work() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for ok in [
        &["test"][..],
        &["check"],
        &["t"],
        &["c"],
        &["--color", "always", "test", "-p", "crate", "a filter"],
        &["-q", "check", "--workspace", "--all-targets"],
        &["test", "--", "--test-threads=2", "filter with spaces"],
    ] {
        assert_eq!(eligible_with_engine(&v(ok), RemoteCargoEngine::Blaze), Ok(true), "{ok:?}");
    }
    for no in [
        &["build"][..],
        &["run"],
        &["clippy"],
        &["metadata"],
        &["test", "--release"],
        &["check", "--profile", "release"],
        &["test", "--target", "aarch64-apple-darwin"],
        &["check", "--target-dir", "/other/cache"],
        &["test", "--config", "build.target-dir='/other/cache'"],
        &["+nightly", "test"],
        &["test", "-Z", "unstable-options"],
        &["check", "--offline"],
        &["test", "--unknown-unsafe-option"],
        &["check", "--", "--cfg", "custom"],
        &[],
    ] {
        assert_eq!(eligible_with_engine(&v(no), RemoteCargoEngine::Blaze), Ok(false), "{no:?}");
    }
    let cargo_args = v(&["clippy", "--all-targets", "-p", "crate"]);
    assert_eq!(eligible_with_engine(&cargo_args, RemoteCargoEngine::Cargo).unwrap(), eligible(&cargo_args));
    assert_eq!(ensure_engine_enabled(RemoteCargoEngine::Cargo, &v(&["build"])), Ok(()), "Cargo is unconditionally enabled");
    assert_eq!(ensure_engine_enabled(RemoteCargoEngine::Blaze, &v(&["check"])), Ok(()));
    assert_eq!(ensure_engine_enabled(RemoteCargoEngine::Blaze, &v(&["clippy"])), Err(BLAZE_ARGS_UNSUPPORTED_MESSAGE));
    assert!(matches!(
        decide_offload_with_engine(Path::new("/missing/config.toml"), &v(&["test"]), Vec::<(String, String)>::new(), RemoteCargoEngine::Blaze),
        Ok(Offload::Local(None))
    ));

    let missing = Path::new("/missing/config.toml");
    let valid = v(&["test"]);
    let unsafe_args = v(&["test", "--release"]);
    assert_eq!(
        run_command_cli(missing, None, Path::new("/missing/cwd"), &valid, None, true, RemoteCargoEngine::Blaze),
        EXIT_ENGINE_NOT_ENABLED,
        "--list-hosts passes empty argv to the guard and fails closed"
    );
    assert_eq!(
        run_command_cli(missing, None, Path::new("/missing/cwd"), &unsafe_args, None, false, RemoteCargoEngine::Blaze),
        EXIT_ENGINE_NOT_ENABLED,
        "unsafe explicit Blaze input is a hard error, never local-decline 125"
    );
    for rejected in [v(&["build"]), v(&["run"]), v(&["clippy"]), v(&["unknown"]), v(&["test", "--unknown-unsafe-option"])] {
        assert_eq!(
            run_command_cli(missing, None, Path::new("/missing/cwd"), &rejected, None, false, RemoteCargoEngine::Blaze),
            EXIT_ENGINE_NOT_ENABLED,
            "rejected explicit Blaze argv must fail before config/host work: {rejected:?}"
        );
    }
    assert_eq!(
        run_cli_with_engine(missing, None, Path::new("/missing/cwd"), &unsafe_args, None, RemoteCargoEngine::Blaze),
        EXIT_ENGINE_NOT_ENABLED,
        "unsafe explicit Blaze input is rejected before reading config"
    );
    assert_eq!(
        run_offload_on_with_id_and_engine(
            &[],
            Path::new("/missing/data"),
            Path::new("/missing/cwd"),
            &unsafe_args,
            &Deadline::new(1),
            "test-call",
            RemoteCargoEngine::Blaze,
        ),
        EXIT_ENGINE_NOT_ENABLED,
        "low-level dispatch rejects unsafe input before host or lease acquisition"
    );
    assert!(run_script_with_engine("/r/job", "", &LeaseToken::new(), 1, 0, RemoteCargoEngine::Blaze, &unsafe_args).is_err());
}

#[test]
fn cargo_engine_uses_the_legacy_remote_script_unchanged() {
    let args = vec!["test".into(), "-p".into(), "xtask".into()];
    let token = LeaseToken::parse("feedfacefeedfacefeedfacefeedface").unwrap();
    // Golden wire script from before the engine protocol, independent of either wrapper.
    let expected = r#"d=/r/job; g=$(ps -o pgid= -p $$ | tr -d ' ') && printf '%s\n' "$g" > "$d.pgid-feedfacefeedfacefeedfacefeedface" && [ "$(cat "$d.owner" 2>/dev/null)" = feedfacefeedfacefeedfacefeedface ] || { echo 'agents-manager: 遠端工作目錄已經還回去了（helper 中途被砍？），不跑 cargo' >&2; exit 126; }; cd "$d"/'' && PATH="$HOME/.cargo/bin:$PATH" CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=4 env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR cargo --config 'build.target-dir="/r/job/target"' --config 'build.build-dir="/r/job/target"' test -p xtask"#;
    assert_eq!(run_script("/r/job", "", &token, 2, 4, &args), expected);
    assert_eq!(
        run_script_with_engine("/r/job", "", &token, 2, 4, RemoteCargoEngine::Cargo, &args).unwrap(),
        expected
    );
}

#[test]
fn blaze_script_uses_host_local_guarded_tools_and_propagates_fake_cargo_failure_once() {
    let base = crate::testing::track(std::env::temp_dir().join(format!("am-blaze-script-{}", crate::db::ulid())));
    let home = base.join("remote home");
    let remote = base.join("remote source with spaces");
    let runtime_base = home.join("host runtime with spaces");
    let cargo_bin = home.join(".cargo/bin");
    let blaze_bin = home.join(".local/share/agents-manager/blaze/bin");
    std::fs::create_dir_all(&cargo_bin).unwrap();
    std::fs::create_dir_all(&blaze_bin).unwrap();
    std::fs::create_dir_all(&remote).unwrap();

    let token = LeaseToken::parse("feedfacefeedfacefeedfacefeedface").unwrap();
    let owner = PathBuf::from(format!("{}.owner", remote.display()));
    std::fs::write(owner, token.as_str()).unwrap();
    let rustc = home.join(".rustup/toolchains/1.99.0/bin/rustc");
    std::fs::create_dir_all(rustc.parent().unwrap()).unwrap();
    crate::testing::write_exec(&rustc, "#!/bin/sh\nexit 0\n");

    let args_log = base.join("cargo args.log");
    let env_log = base.join("cargo env.log");
    let count_log = base.join("cargo calls.log");
    crate::testing::write_exec(
        cargo_bin.join("rustup"),
        "#!/bin/sh\n[ \"$*\" = 'which --toolchain 1.99.0 rustc' ] || exit 39\nprintf '%s\\n' \"$FAKE_RUSTC\"\n",
    );
    crate::testing::write_exec(
        cargo_bin.join("cargo"),
        format!(r#"#!/bin/sh
if [ "$1" = "--list" ]; then
  selected="$AGM_BLAZE_BIN/cargo-blaze"
  [ ! -x "$CARGO_HOME/bin/cargo-blaze" ] || selected="$CARGO_HOME/bin/cargo-blaze"
  if [ -f .cargo/config.toml ] && grep -q 'blaze =' .cargo/config.toml; then selected="alias: version"; fi
  printf 'Installed Commands:\n    blaze %s\n' "$selected"
  exit 0
fi
printf 'call\n' >> '{count}'
printf '%s\n' "$@" > '{args}'
printf '%s\n' "$RUSTUP_TOOLCHAIN" "$RA_JIT_HOME" "$BLAZE_HOME" "$RA_JIT_RUSTC_BASE" "$BLAZE_RUSTC_BASE" "$CARGO_BUILD_JOBS" "$RUST_TEST_THREADS" "$RUSTC" "$CARGO_BUILD_RUSTC" "wrapper:$RUSTC_WRAPPER" "workspace:$RUSTC_WORKSPACE_WRAPPER" "cargo-wrapper:$CARGO_BUILD_RUSTC_WRAPPER" "cargo-workspace:$CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER" > '{env}'
exit "${{FAKE_EXIT:-0}}"
"#, count = count_log.display(), args = args_log.display(), env = env_log.display()),
    );
    crate::testing::write_exec(blaze_bin.join("cargo"), "#!/bin/sh\nexit 96\n");
    crate::testing::write_exec(blaze_bin.join("rustup"), "#!/bin/sh\nexit 95\n");
    crate::testing::write_exec(blaze_bin.join("cargo-blaze"), "#!/bin/sh\nexit 0\n");
    crate::testing::write_exec(blaze_bin.join("ra-jit"), "#!/bin/sh\nexit 0\n");
    std::fs::write(blaze_bin.join(".agm-blaze-memory-v1"), "verified fixture\n").unwrap();

    let script = run_script_with_engine(
        &remote.display().to_string(),
        "",
        &token,
        3,
        5,
        RemoteCargoEngine::Blaze,
        &[
            "test".into(),
            "-p".into(),
            "package with spaces".into(),
            "filter 'with quotes'".into(),
            String::new(),
            "--".into(),
            "--test-threads=2".into(),
        ],
    )
    .unwrap();

    let run_with_options = |rustc_path: &Path, fake_exit: &str, cargo_home: &Path, alias: &str| {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("FAKE_RUSTC", rustc_path)
            .env("FAKE_EXIT", fake_exit)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_ALIAS_BLAZE", alias)
            .env("RUSTC", "/inherited/wrong-rustc")
            .env("CARGO_BUILD_RUSTC", "/inherited/wrong-cargo-rustc")
            .env("RUSTC_WRAPPER", "/inherited/wrapper")
            .env("RUSTC_WORKSPACE_WRAPPER", "/inherited/workspace-wrapper")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "/inherited/cargo-wrapper")
            .env("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", "/inherited/cargo-workspace-wrapper")
            .env("RA_JIT_HOME", "/inherited/old-ra")
            .env("BLAZE_HOME", "/inherited/old-blaze")
            .env("RA_JIT_RUSTC_BASE", "1")
            .env("BLAZE_RUSTC_BASE", "1")
            .env("AGM_BLAZE_BIN", &blaze_bin)
            .env("AGM_BLAZE_RUNTIME_HOME", &runtime_base)
            .env("FAKE_ARGS_LOG", &args_log)
            .env("FAKE_ENV_LOG", &env_log)
            .env("FAKE_COUNT_LOG", &count_log)
            .output()
            .unwrap()
    };

    let run_with_cargo_home = |rustc_path: &Path, fake_exit: &str, cargo_home: &Path| {
        run_with_options(rustc_path, fake_exit, cargo_home, "")
    };
    let run = |rustc_path: &Path, fake_exit: &str| {
        run_with_cargo_home(rustc_path, fake_exit, &home.join(".cargo"))
    };

    // Cargo searches CARGO_HOME/bin before PATH, even with the verified private bin first.
    let unexpected_plugin = cargo_bin.join("cargo-blaze");
    crate::testing::write_exec(&unexpected_plugin, "#!/bin/sh\nexit 98\n");
    let out = run(&rustc, "0");
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unverified Blaze plugin"));
    assert!(!args_log.exists(), "unverified default Cargo-home plugin must fail before Cargo starts");
    std::fs::remove_file(&unexpected_plugin).unwrap();

    let alternate_cargo_home = base.join("alternate cargo home");
    std::fs::create_dir_all(alternate_cargo_home.join("bin")).unwrap();
    crate::testing::write_exec(alternate_cargo_home.join("bin/cargo-blaze"), "#!/bin/sh\nexit 99\n");
    let out = run_with_cargo_home(&rustc, "0", &alternate_cargo_home);
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unverified Blaze plugin"));
    assert!(!args_log.exists(), "custom Cargo-home plugin must not bypass private-tool verification");

    let out = run_with_options(&rustc, "0", &home.join(".cargo"), "version");
    assert_eq!(out.status.code(), Some(127));
    assert!(!args_log.exists(), "inherited Cargo alias must fail before a command executes");
    std::fs::create_dir_all(remote.join(".cargo")).unwrap();
    let alias_config = remote.join(".cargo/config.toml");
    std::fs::write(&alias_config, "[alias]\nblaze = \"version\"\n").unwrap();
    let out = run(&rustc, "0");
    assert_eq!(out.status.code(), Some(127));
    assert!(!args_log.exists(), "configured alias must not replace the verified plugin");
    std::fs::remove_file(alias_config).unwrap();

    let missing_rustc = base.join("missing rustc");
    let out = run(&missing_rustc, "0");
    assert_eq!(out.status.code(), Some(127), "missing Rust 1.99.0 must be a hard failure");
    assert!(!args_log.exists(), "missing toolchain must fail before Cargo starts");

    std::fs::remove_file(blaze_bin.join(".agm-blaze-memory-v1")).unwrap();
    let out = run(&rustc, "0");
    assert_eq!(out.status.code(), Some(127), "missing capability marker must be a hard failure");
    assert!(!args_log.exists(), "missing capability must fail before Cargo starts");
    std::fs::write(blaze_bin.join(".agm-blaze-memory-v1"), "verified fixture\n").unwrap();

    std::fs::remove_file(blaze_bin.join("cargo-blaze")).unwrap();
    let out = run(&rustc, "0");
    assert_eq!(out.status.code(), Some(127), "missing private plugin must be a hard failure");
    assert!(!args_log.exists(), "missing plugin must fail before Cargo starts");
    crate::testing::write_exec(blaze_bin.join("cargo-blaze"), "#!/bin/sh\nexit 0\n");

    std::fs::write(remote.join(".cargo/config.toml"), "[build]\nrustc = \"/configured/wrong-rustc\"\nrustc-wrapper = \"/configured/wrong-wrapper\"\nrustc-workspace-wrapper = \"/configured/wrong-workspace-wrapper\"\n").unwrap();
    let out = run(&rustc, "37");
    assert_eq!(out.status.code(), Some(37), "Blaze/Cargo failures must preserve the original exit status");
    assert_eq!(std::fs::read_to_string(&count_log).unwrap().lines().count(), 1, "a failed remote command must not be retried locally");
    let got_args = std::fs::read_to_string(&args_log).unwrap().split_terminator('\n').map(str::to_string).collect::<Vec<_>>();
    let cache = format!("build.target-dir=\"{}/target\"", remote.display());
    let build = format!("build.build-dir=\"{}/target\"", remote.display());
    assert_eq!(
        got_args,
        vec![
            "--config".to_string(), format!("build.rustc=\"{}\"", rustc.display()), "--config".into(), "build.rustc-wrapper=\"\"".into(), "--config".into(), "build.rustc-workspace-wrapper=\"\"".into(), "--config".to_string(), cache, "--config".to_string(), build, "blaze".to_string(), "test".to_string(), "-p".to_string(),
            "package with spaces".to_string(), "filter 'with quotes'".to_string(), String::new(), "--".to_string(), "--test-threads=2".to_string(),
        ]
    );
    let got_env = std::fs::read_to_string(&env_log).unwrap().lines().map(str::to_string).collect::<Vec<_>>();
    let namespace = runtime_base.join("native-base-v1").display().to_string();
    assert_eq!(
        got_env,
        vec!["1.99.0".to_string(), namespace.clone(), namespace, "0".into(), "0".into(), "3".into(), "5".into(), rustc.display().to_string(), rustc.display().to_string(), "wrapper:".into(), "workspace:".into(), "cargo-wrapper:".into(), "cargo-workspace:".into()]
    );
    assert!(!got_env.iter().any(|v| v.contains("inherited")), "host-local aliases must replace inherited paths");
    let out = run(&rustc, "0");
    assert_eq!(out.status.code(), Some(0), "verified fake Cargo success must propagate");
    assert_eq!(std::fs::read_to_string(&count_log).unwrap().lines().count(), 2, "only the two explicit fake invocations may run");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn cargo_command_handler_forwards_list_hosts_and_native_decline() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-engine-cargo-{}", crate::db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "[build.remote]\nenabled = true\nhost = \"build-host\"\nuser = \"builder\"\n").unwrap();
    let args = vec!["test".to_string()];
    assert!(matches!(decide_offload(&config, &args, Vec::<(String, String)>::new()), Offload::Go(_)));
    assert_eq!(
        run_command_cli(&config, None, &dir, &args, None, true, RemoteCargoEngine::Cargo),
        0,
        "Cargo list-hosts must accept the configured host without acquiring a lease"
    );
    std::fs::write(&config, "[build.remote]\nenabled = false\n").unwrap();
    assert_eq!(
        run_command_cli(&config, None, &dir, &args, None, false, RemoteCargoEngine::Cargo),
        EXIT_RUN_LOCALLY,
        "Cargo must retain the normal not-started decline path"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn shell_quoting_does_not_allow_argument_injection() {
    assert_eq!(sh_quote("abc/def"), "abc/def");
    assert_eq!(sh_quote("x; touch /tmp/pwn"), "'x; touch /tmp/pwn'");
    assert_eq!(sh_quote("a'b"), "'a'\\''b'");
}

const ROOT: &str = "/home/u/.cache/agents-manager/remote-cargo";

fn rdir(leaf: &str, locked: bool, busy: bool, idle_secs: u64) -> RemoteDir {
    RemoteDir { path: format!("{ROOT}/{leaf}"), locked, busy, idle_secs }
}

fn hs(dirs: Vec<RemoteDir>, disk: Option<(u64, u64)>) -> Handshake {
    Handshake {
        root: ROOT.into(),
        dir: format!("{ROOT}/0123456789abcdef/shared"),
        token: "0123456789abcdef0123456789abcdef".into(),
        dirs,
        disk,
        slot: None,
    }
}

/// #141：孤兒（沒鎖、沒行程、放超過 10 分鐘）要收；有人在用的——鎖被持有、有行程 cwd 在裡面——
/// 放多久都不能碰；自己這次的目錄也不能碰。
#[test]
fn gc_picks_idle_orphans_and_never_touches_a_directory_in_use() {
    let old = ORPHAN_IDLE_SECS + 1;
    let h = hs(
        vec![
            rdir("00000000000000aa/111", false, false, old),         // 舊版留下的 <pid>，閒置 → 收
            rdir("00000000000000aa/job-9-1", false, false, old),     // 守門死在遠端的 job → 收
            rdir("00000000000000aa/222", false, true, old * 50),     // 有行程在裡面 → 不收
            rdir("00000000000000aa/job-9-2", true, false, old * 50), // 鎖被持有 → 不收
            rdir("00000000000000aa/333", false, false, ORPHAN_IDLE_SECS - 1), // 剛動過 → 不收
            rdir("0123456789abcdef/shared", false, false, old * 50), // 自己這次的 → 不收
        ],
        Some((80, 100)),
    );
    assert_eq!(
        select_gc(&h, &GcPolicy::default()),
        vec![format!("{ROOT}/00000000000000aa/111"), format!("{ROOT}/00000000000000aa/job-9-1")]
    );
}

/// `shared/` 是快取：平常閒置 3 小時才收；磁碟剩不到 25% 時照孤兒的 10 分鐘收（#141 塞滿 97G）。
#[test]
fn a_shared_target_is_kept_as_cache_unless_the_disk_is_running_out() {
    let dirs = vec![
        rdir("00000000000000aa/shared", false, false, ORPHAN_IDLE_SECS + 1),
        rdir("00000000000000bb/shared", false, false, SHARED_IDLE_SECS + 1),
        rdir("00000000000000cc/shared", true, false, SHARED_IDLE_SECS * 10),
    ];
    let roomy = select_gc(&hs(dirs.clone(), Some((50, 100))), &GcPolicy::default());
    assert_eq!(roomy, vec![format!("{ROOT}/00000000000000bb/shared")]);
    let full = select_gc(&hs(dirs, Some((10, 100))), &GcPolicy::default());
    assert_eq!(
        full,
        vec![format!("{ROOT}/00000000000000aa/shared"), format!("{ROOT}/00000000000000bb/shared")],
        "磁碟緊時，被持有的 shared 還是不能收"
    );
}

/// issue #196：只靠「閒置 3 小時」擋不住——一天開十幾顆子 agent、每張票數個變異副本，每個路徑一個新的 hash、各 2～3G（實測 36 個 hash、29G）。
/// 閒置時間可設定；另外  最多留 N 份，超過就從最久沒用的開始收（LRU）。鎖被持有、有行程在用、這次自己的不算「可收」，
/// 而且剛用完的（不到 10 分鐘）不動——不然一次跑完的變異副本會被下一個呼叫立刻收走、白白冷編譯。
#[test]
fn shared_targets_are_reclaimed_by_a_configurable_age_and_by_a_count_cap_oldest_first() {
    let h = |n: &str| format!("{ROOT}/00000000000000{n}/shared");
    let dirs = vec![
        rdir("00000000000000a1/shared", false, false, 7 * 3600), // 7 小時：超過 6 小時 → 依時間收
        rdir("00000000000000a2/shared", false, false, 5 * 3600),
        rdir("00000000000000a3/shared", false, false, 4 * 3600),
        rdir("00000000000000a4/shared", false, false, 3 * 3600),
        rdir("00000000000000a5/shared", false, false, 2 * 3600),
        rdir("00000000000000aa/shared", false, false, 3600),
        rdir("00000000000000a6/shared", false, false, 30 * 60),
        rdir("00000000000000a7/shared", false, false, 5 * 60), // 剛用完：就算超過數量上限也不動
        rdir("00000000000000a8/shared", true, false, 20 * 3600), // 鎖被持有：不動
        rdir("00000000000000a9/shared", false, true, 20 * 3600), // 有行程在裡面：不動
    ];
    let pick = |policy: &GcPolicy| {
        let mut got = select_gc(&hs(dirs.clone(), Some((80, 100))), policy);
        got.sort();
        got
    };
    let names = |ns: &[&str]| ns.iter().map(|n| h(n)).collect::<Vec<_>>();
    // 共 10 份＋這次自己的一份。數量不限：只依時間收超過 6 小時的。
    assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 0 }), names(&["a1"]));
    // 上限 10：依時間收掉 a1 之後剩 10 份，剛好在上限內。
    assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 10 }), names(&["a1"]));
    // 上限 6：還要再收 4 份——**最久沒用的先收**（a2、a3、a4、a5），不是最新的。
    assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 6 }), names(&["a1", "a2", "a3", "a4", "a5"]));
    // 上限 1：能收的都收（a2～a6、aa），但不碰鎖住的（a8）、有行程的（a9）、**剛用完的（a7）**、也不碰自己——留下的比上限多也沒關係。
    assert_eq!(pick(&GcPolicy { shared_idle_secs: 6 * 3600, max_shared: 1 }), names(&["a1", "a2", "a3", "a4", "a5", "a6", "aa"]));
}

/// 政策從設定來：閒置小時數至少算 1（0 不能變成「馬上收光」），預設 3 小時、最多 12 份（issue #417），舊設定沒寫這兩個 key 也是預設。
#[test]
fn the_gc_policy_comes_from_the_remote_settings() {
    assert_eq!(GcPolicy::default(), GcPolicy { shared_idle_secs: 3 * 3600, max_shared: 12 });
    let cfg = BuildRemoteCfg { shared_idle_hours: 0, max_shared_dirs: 3, ..Default::default() };
    assert_eq!(GcPolicy::from_cfg(&cfg), GcPolicy { shared_idle_secs: 3600, max_shared: 3 });
    let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
    assert_eq!(GcPolicy::from_cfg(&old), GcPolicy::default());
}

/// remote_root 設錯（例如設成家目錄）也只碰自己命名規則的目錄。
#[test]
fn gc_only_ever_names_directories_in_its_own_layout() {
    let old = SHARED_IDLE_SECS * 10;
    let foreign = [
        "/etc/00000000000000aa/111".to_string(),
        format!("{ROOT}-other/00000000000000aa/111"),
        format!("{ROOT}/nothex/111"),
        format!("{ROOT}/00000000000000AA/111"),
        format!("{ROOT}/00000000000000aa/src"),
        format!("{ROOT}/00000000000000aa/12-3"),
        format!("{ROOT}/00000000000000aa/job-"),
        format!("{ROOT}/00000000000000aa/111/nested"),
        format!("{ROOT}/00000000000000aa/.."),
    ];
    let dirs = foreign
        .iter()
        .map(|p| RemoteDir { path: p.clone(), locked: false, busy: false, idle_secs: old })
        .collect();
    assert!(select_gc(&hs(dirs, Some((1, 100))), &GcPolicy::default()).is_empty());
    assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/shared")), Some(LeafKind::Shared));
    assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/job-12-34")), Some(LeafKind::Job));
    assert_eq!(leaf_kind(ROOT, &format!("{ROOT}/00000000000000aa/4711")), Some(LeafKind::Job));
}

#[test]
fn the_handshake_skips_login_noise_and_fails_if_the_guard_dies_first() {
    // 排隊時的心跳（`Q`）也是雜訊，略過。
    let out = "Welcome!\nQ\nQ\nROOT\t/r\nDIR\t/r/0123456789abcdef/shared\tfeedfacefeedfacefeedfacefeedface\nSLOT\t3\t6\n\
                   D\t/r/00000000000000aa/111\t0\t1\t-3\nD\t/r/00000000000000aa/222\t1\t0\t900\n\
                   F\t500\t1000\nEND\nD\t/late\t0\t0\t9\n";
    let h = read_handshake(&mut std::io::Cursor::new(out)).unwrap();
    assert_eq!(h.slot, Some((3, 6)));
    assert_eq!(h.root, "/r");
    assert_eq!(h.dir, "/r/0123456789abcdef/shared");
    assert_eq!(h.token, "feedfacefeedfacefeedfacefeedface");
    assert_eq!(h.disk, Some((500, 1000)));
    assert_eq!(
        h.dirs,
        vec![
            RemoteDir { path: "/r/00000000000000aa/111".into(), locked: false, busy: true, idle_secs: 0 },
            RemoteDir { path: "/r/00000000000000aa/222".into(), locked: true, busy: false, idle_secs: 900 },
        ]
    );
    // 守門在交出目錄前就死了（沒 flock、磁碟滿…）：不能當成拿到目錄。
    assert!(read_handshake(&mut std::io::Cursor::new("ROOT\t/r\nEND\n")).is_err());
    assert!(read_handshake(&mut std::io::Cursor::new("")).is_err());
}

/// run 要先記 pgid、再核 owner、最後才跑 cargo：守門清理是「先撤 owner 再讀 pgid」，順序反了會漏收。
#[test]
fn the_remote_run_records_its_group_before_checking_the_lease() {
    let tok = LeaseToken::parse("0123456789abcdef0123456789abcdef").unwrap();
    let s = run_script("/r/0123456789abcdef/shared", "", &tok, 4, 8, &["test".into(), "-p".into(), "x; rm -rf ~".into()]);
    let pgid = s.find("> \"$d.pgid-0123456789abcdef0123456789abcdef\"").expect(&s);
    let owner = s.find("\"$d.owner\"").expect(&s);
    let cargo = s.find("cargo --config").expect(&s);
    assert!(pgid < owner && owner < cargo, "{s}");
    assert!(s.contains("exit 126"), "{s}");
    assert!(s.contains("'x; rm -rf ~'"), "{s}");
    assert!(s.contains("PATH=\"$HOME/.cargo/bin:$PATH\" CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=8 env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR cargo --config"), "{s}");
    assert!(s.contains("build.target-dir=\"/r/0123456789abcdef/shared/target\""), "{s}");
    assert!(s.contains("build.build-dir=\"/r/0123456789abcdef/shared/target\""), "{s}");
    assert!(!s.contains("CARGO_TARGET_DIR="), "nested Cargo calls must not inherit this cache: {s}");
    assert!(s.contains("env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR"), "remote cache environment must not escape the lease: {s}");
    // 0＝不設：不帶這個環境變數（libtest 用自己的預設）。
    let s = run_script("/r/0123456789abcdef/shared", "", &tok, 4, 0, &["test".into()]);
    assert!(!s.contains("RUST_TEST_THREADS"), "{s}");
}

/// issue #202：遠端是 32 vCPU 的超賣主機，測試預設開 32 個執行緒反而更慢（283 秒 vs 限 8 個 183 秒）。預設 8、可設定；呼叫端自己設了
/// `RUST_TEST_THREADS` 就尊重它（`--test-threads` 命令列旗標本來就優先於環境變數）；`0`＝不設。
#[test]
fn the_remote_test_thread_cap_defaults_to_eight_and_yields_to_the_caller() {
    assert_eq!(BuildRemoteCfg::default().test_threads, 8);
    let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
    assert_eq!(old.test_threads, 8, "舊設定沒有這個 key：用預設");
    assert_eq!(pick_test_threads(None, 8), 8);
    assert_eq!(pick_test_threads(Some("4"), 8), 4, "呼叫端自己設的優先");
    assert_eq!(pick_test_threads(Some(" 16 "), 8), 16);
    assert_eq!(pick_test_threads(Some("0"), 8), 8, "0 或看不懂的不算數");
    assert_eq!(pick_test_threads(Some("many"), 8), 8);
    assert_eq!(pick_test_threads(None, 0), 0, "設定成 0＝不設");
}

/// #148：租約身份固定格式（32 個小寫 hex），嵌進遠端 shell 與檔名都不會變成 injection；
/// 每次都是新的——不是 PID、不是時間，撞不到。
#[test]
fn a_lease_token_is_fixed_format_random_and_never_repeats() {
    let a = LeaseToken::new();
    assert_eq!(a.as_str().len(), 32);
    assert!(a.as_str().bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')), "{a:?}");
    assert_eq!(LeaseToken::parse(a.as_str()), Some(a.clone()));
    let seen: std::collections::HashSet<String> = (0..2000).map(|_| LeaseToken::new().as_str().to_string()).collect();
    assert_eq!(seen.len(), 2000, "隨機 token 不能重複");

    for bad in [
        "",
        "4242",                                     // 舊版的 `$$`
        "0123456789ABCDEF0123456789ABCDEF",         // 大寫
        "0123456789abcdef0123456789abcde",          // 少一個
        "0123456789abcdef0123456789abcdef0",        // 多一個
        "0123456789abcdef0123456789abcde;",         // shell 字元
        "../../../../../../../../../../etc/passwd", // 路徑穿越
        "0123456789abcdef0123456789abcd\n",         // 換行
        "0123456789abcdef0123456789abcd'x",         // 引號
    ] {
        assert!(LeaseToken::parse(bad).is_none(), "{bad:?}");
    }
    let script = guard_script("/r", "0123456789abcdef", "job-1-1", &a, Admission::from_cfg(&BuildRemoteCfg::default()));
    assert!(script.contains(&format!("token={}", a.as_str())), "token 要原樣、不帶引號地進腳本");
    assert!(!script.contains("@TOKEN@"), "沒替換乾淨");
}

/// 守門交回來的 token 不是我們給的那個＝出事了，不能當成拿到目錄（不然後面 run 會拿錯身份去核 owner）。
#[test]
fn a_guard_that_answers_with_another_token_is_not_a_lease() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("printf 'ROOT\\t/r\\nDIR\\t/r/0123456789abcdef/shared\\tfeedfacefeedfacefeedfacefeedface\\nEND\\n'; cat >/dev/null");
    let err = Lease::start(cmd, LeaseToken::new(), &Deadline::new(0)).err().expect("token mismatch must fail").to_string();
    assert!(err.contains("lease token"), "{err}");
}

/// #1971：`@LOADAVG@` 代入正式路徑（`/proc/loadavg`，經 `sh_quote` 不加引號）後，守門腳本跟改動前逐 byte 相同。
/// 基準是改動前用同一組輸入組出的 sha256（試用與排隊各一份）；腳本變了就要先確認是不是這裡的預期。
#[test]
fn the_production_guard_script_is_byte_identical_to_before_the_loadavg_placeholder() {
    use sha2::Digest as _;
    let tok = LeaseToken::parse("0123456789abcdef0123456789abcdef").unwrap();
    let trial = Admission { max: 3, jobs: 5, shared_wait_secs: 900, queue_wait_secs: 3, pool: 0, try_only: true, max_load: Some(12.5) };
    let queued = Admission { max: 0, jobs: 12, shared_wait_secs: 900, queue_wait_secs: 3, pool: 0, try_only: false, max_load: None };
    for (adm, want) in [
        (trial, "0788dfca10036aa8ceb1c9ecda4ad4083b957887517ffc848ad71c11564ae69b"),
        (queued, "618a3cac3c3cae40f5a55e096c5e41de99304fe23d83b74e5d5ec98ef8238f93"),
    ] {
        let s = guard_script("/r", "0123456789abcdef", "job-1-1", &tok, adm);
        let got: String = sha2::Sha256::digest(s.as_bytes()).iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(got, want, "守門腳本的 byte 變了");
        assert!(s.contains("awk -v m=\"$maxload\" '{ exit !($1 > m) }' /proc/loadavg 2>/dev/null"), "正式路徑要讀 /proc/loadavg");
        assert!(!s.contains("@LOADAVG@"), "沒替換乾淨");
    }
}

/// 守門腳本裡那條自動上限算式的 Rust 版（測試用的對照；遠端才知道自己的核數與 RAM，正式的只有腳本那份）。
pub(super) fn auto_max_concurrent(ncpu: u64, mem_kb: u64, jobs: usize) -> usize {
    let by_cpu = ncpu * 3 / jobs.max(1) as u64;
    let by_mem = mem_kb.saturating_sub(AUTO_RESERVE_KB) / AUTO_PER_JOB_KB;
    by_cpu.min(by_mem).max(1) as usize
}

/// issue #104／#1956：`max_concurrent = 0`（預設）依遠端核數與 RAM 算——CPU 容許 3 倍超賣，記憶體每個編譯 14 GiB（`agents_managerd`
/// 那顆 rustc 實測 13.5 GB）、留 8 GiB 給系統。本機（16 核／62 GiB）配 `cargo_jobs = 12` 是 3；小機器至少 1；設定寫了數字就用設定、最多 64。
#[test]
fn the_remote_cap_defaults_to_what_the_hosts_cores_and_ram_can_take() {
    assert_eq!(BuildRemoteCfg::default().max_concurrent, 0, "預設＝自動");
    let old: BuildRemoteCfg = toml::from_str("host = \"h\"\n").unwrap();
    assert_eq!(old.max_concurrent, 0, "舊設定沒有這個 key：自動");
    assert_eq!(auto_max_concurrent(16, 65_000_000, 12), 3, "本機：min(16×3÷12, (62−8)÷14) = min(4, 3)");
    assert_eq!(auto_max_concurrent(16, 65_000_000, 24), 2, "cargo_jobs 很大時換 CPU 卡住");
    assert_eq!(auto_max_concurrent(8, 15 * 1024 * 1024, 8), 1, "r7（15 GiB）：一次一個");
    assert_eq!(auto_max_concurrent(2, 4 * 1024 * 1024, 8), 1, "再小也至少 1");
    assert_eq!(auto_max_concurrent(64, 256 * 1024 * 1024, 4), 17);
    let set = BuildRemoteCfg { max_concurrent: 3, cargo_jobs: 5, ..Default::default() };
    let adm = Admission::from_cfg(&set);
    assert_eq!((adm.max, adm.jobs, adm.shared_wait_secs, adm.queue_wait_secs), (3, 5, SHARED_WAIT_SECS, QUEUE_WAIT_SECS));
    let script = guard_script("/r", "0123456789abcdef", "job-1-1", &LeaseToken::new(), adm);
    for want in [&format!("max=3; jobs=5; swait={SHARED_WAIT_SECS}; qwait=3"), &format!("(mem - {AUTO_RESERVE_KB}) / {AUTO_PER_JOB_KB}")] {
        assert!(script.contains(want), "{want}");
    }
    assert!(!script.contains('@'), "沒替換乾淨");
    // #1937：24 小時統計要 grep 的四種字樣（拿到 shared 並等了幾秒、shared 逾時、改用獨立目錄、轉本機）都在守門裡，彼此分得開。
    for want in [
        "等了 $hw 秒拿到共用遠端 target",
        "秒共用遠端 target 還沒空出來",
        "這次改用獨立目錄（冷編譯，結束就刪）",
        "這次改在本機排隊",
    ] {
        assert!(script.contains(want), "{want}");
    }
    assert!(!script.contains("拿到遠端名額"), "shared 鎖的字樣不能跟名額那層（「排到遠端名額」）混在一起");
}

/// issue #104：排隊時守門可能很久才交出目錄，這段時間收到終止訊號（Ctrl-C／TERM）也要馬上停，不能卡到排到名額為止。
#[test]
fn a_queued_handshake_still_honours_a_termination_signal() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("exec sleep 60");
    let ctl = Deadline::new(0);
    let abort = ctl.abort.clone();
    let flip = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        abort.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let t0 = std::time::Instant::now();
    let err = Lease::start(cmd, LeaseToken::new(), &ctl).err().expect("interrupted");
    flip.join().unwrap();
    assert!(matches!(err.downcast_ref::<Stopped>(), Some(Stopped::Interrupted(_))), "{err:#}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(50), "{:?}", t0.elapsed());
}

/// #323：cargo shim 被單獨殺掉（沒有人會再送訊號）時，helper 要發現父行程沒了、把遠端收乾淨，不能孤兒似地跑到 timeout。
#[test]
fn an_orphaned_helper_stops_when_its_parent_shim_is_gone() {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("exec sleep 60");
    let mut ctl = Deadline::new(0);
    ctl.parent = Some((4242, || 1)); // 啟動時的父行程是 4242，現在變成 1（被 init 收養）
    let t0 = std::time::Instant::now();
    let err = Lease::start(cmd, LeaseToken::new(), &ctl).err().expect("interrupted");
    assert!(matches!(err.downcast_ref::<Stopped>(), Some(Stopped::Interrupted(_))), "{err:#}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(50), "{:?}", t0.elapsed());
    let mut same = Deadline::new(0);
    same.parent = Some((7, || 7));
    assert!(same.stop().is_none(), "父行程沒變就照常跑");
}

fn secret_dir() -> PathBuf {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-cargo-{}", crate::db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// data-dir 裡的密碼檔（含暫存檔）：(檔名, 權限)。
fn password_files(dir: &Path) -> Vec<(String, u32)> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut v: Vec<(String, u32)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(PASSWORD_FILE))
        .map(|n| {
            let mode = std::fs::metadata(dir.join(&n)).unwrap().permissions().mode() & 0o777;
            (n, mode)
        })
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn password_file_is_private_and_metadata_does_not_echo_the_secret() {
    let dir = secret_dir();
    let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
    let remote = commit_remote(&store, &dir, Some("super-secret"), |_| {}).await.unwrap();
    assert!(password_is_set(&dir, &remote));
    let files = password_files(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].1, 0o600, "{files:?}");
    let v = sanitized(&remote, &dir);
    assert_eq!(v["password_set"], true);
    assert!(!v.to_string().contains("super-secret"));
    assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("super-secret"), "密碼不進 config.toml");
    let _ = std::fs::remove_dir_all(dir);
}

/// 舊版設定（沒有 `password_id`）照舊讀固定檔名；`None`＝沿用（換主機也一樣）、`Some("")`＝清掉、新密碼＝換一份新檔、舊檔刪掉。
/// 設定明確指到一份密碼檔而它不見了是錯誤，不能悄悄改用 key/agent；手改壞的 id 不能變成路徑。
#[tokio::test]
async fn saving_settings_keeps_clears_or_replaces_the_password_as_asked() {
    let dir = secret_dir();
    let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
    std::fs::write(dir.join(PASSWORD_FILE), "legacy-pw\n").unwrap();
    let legacy = store.get().await.build.remote;
    assert_eq!(legacy.password_id, None);
    assert_eq!(secret(&dir, &legacy).unwrap().as_deref(), Some("legacy-pw"), "舊版設定：讀固定檔名");

    let r = commit_remote(&store, &dir, None, |r| r.host = "b".into()).await.unwrap();
    assert_eq!((r.host.as_str(), secret(&dir, &r).unwrap().as_deref()), ("b", Some("legacy-pw")), "None＝沿用");
    let names: Vec<String> = password_files(&dir).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec![PASSWORD_FILE.to_string()], "沿用：舊版固定檔名留著");

    let r = commit_remote(&store, &dir, Some("pw2"), |_| {}).await.unwrap();
    let id = r.password_id.clone().expect("新密碼要有自己的 id");
    assert_eq!(secret(&dir, &r).unwrap().as_deref(), Some("pw2"));
    assert_eq!(password_files(&dir), vec![(format!("{PASSWORD_FILE}.{id}"), 0o600)], "舊版固定檔名要刪掉");

    let r = commit_remote(&store, &dir, None, |r| r.host = "c".into()).await.unwrap();
    assert_eq!((r.password_id.as_deref(), secret(&dir, &r).unwrap().as_deref()), (Some(id.as_str()), Some("pw2")));

    let r = commit_remote(&store, &dir, Some(""), |_| {}).await.unwrap();
    assert_eq!(r.password_id.as_deref(), Some(""));
    assert_eq!(secret(&dir, &r).unwrap(), None);
    assert!(!password_is_set(&dir, &r));
    assert!(password_files(&dir).is_empty(), "清掉＝一份密碼檔都不留：{:?}", password_files(&dir));

    let gone = BuildRemoteCfg { password_id: Some("0123456789abcdef".into()), ..Default::default() };
    let err = secret(&dir, &gone).unwrap_err().to_string();
    assert!(err.contains("不見了"), "{err}");
    for bad in ["../../etc/passwd", "0123456789ABCDEF", "123", "0123456789abcdef/"] {
        let r = BuildRemoteCfg { password_id: Some(bad.into()), ..Default::default() };
        assert!(secret(&dir, &r).is_err(), "{bad}");
        assert!(!password_is_set(&dir, &r), "{bad}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// issue #104：換設定＋密碼時，任何一步失敗、或行程死在任何一步之後，重讀磁碟只會是「舊主機＋舊密碼」或「新主機＋新密碼」，
/// 而且 data-dir 裡任何時候都沒有權限比 0600 寬的密碼檔。
/// 以前先改 config 再 `fs::write` 密碼、寫完才 chmod：寫密碼失敗＝新主機配舊密碼（舊密碼會被送去新主機）；
/// write 與 chmod 之間 crash＝密碼檔永遠是 umask 決定的 0644。
#[tokio::test]
async fn a_failure_or_crash_at_any_step_leaves_the_old_or_the_new_pair_never_a_mix() {
    use std::os::unix::fs::PermissionsExt as _;
    for step in ["created", "written", "synced", "renamed", "config", "cleanup"] {
        for how in [Fault::Fail, Fault::Crash] {
            let dir = secret_dir();
            let cfg_path = dir.join("config.toml");
            let store = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap();
            store.update(|c| {
                c.build.remote.host = "old-host".into();
                Ok(())
            })
            .await
            .unwrap();
            let old_file = dir.join(PASSWORD_FILE);
            std::fs::write(&old_file, "old-pw").unwrap();
            std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o600)).unwrap();

            SECRET_FAULT.with(|f| f.set(Some((step, how))));
            let res = commit_remote(&store, &dir, Some("new-pw"), |r| r.host = "new-host".into()).await;
            SECRET_FAULT.with(|f| f.set(None));
            let why = format!("{step}/{how:?}: {res:?}");

            // 重開 daemon：只看磁碟上的東西。
            let fresh = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap().get().await.build.remote;
            let pair = (fresh.host.clone(), secret(&dir, &fresh).unwrap());
            if step == "cleanup" {
                assert_eq!(pair, ("new-host".to_string(), Some("new-pw".to_string())), "已經提交：{why}");
            } else {
                assert_eq!(pair, ("old-host".to_string(), Some("old-pw".to_string())), "還沒提交：{why}");
            }
            assert_eq!(res.is_ok(), step == "cleanup" && how == Fault::Fail, "提交之後清舊檔失敗不算失敗，其他都要回錯：{why}");
            let files = password_files(&dir);
            assert!(files.iter().all(|(_, mode)| *mode == 0o600), "每一份密碼檔（含暫存檔）都要是 0600：{why} {files:?}");
            if how == Fault::Fail && step != "cleanup" {
                assert_eq!(files, vec![(PASSWORD_FILE.to_string(), 0o600)], "失敗要收乾淨，只剩舊檔：{why}");
            }

            // 下一次成功的儲存把殘骸（沒人指到的暫存檔、舊檔）收掉。
            let store = crate::runners::app_ports_p2::load_config(cfg_path.clone()).await.unwrap();
            let r = commit_remote(&store, &dir, Some("newer-pw"), |r| r.host = "newer-host".into()).await.unwrap();
            assert_eq!(secret(&dir, &r).unwrap().as_deref(), Some("newer-pw"), "{why}");
            let id = r.password_id.clone().unwrap();
            assert_eq!(password_files(&dir), vec![(format!("{PASSWORD_FILE}.{id}"), 0o600)], "{why}");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
