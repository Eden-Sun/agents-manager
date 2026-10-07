
    use super::*;
    use std::os::unix::fs::symlink;

    /// 假的 `/proc`：net/tcp、net/tcp6 照核心的欄位排，fd 是指到 `socket:[inode]` 的懸空符號連結（跟真的一樣）。
    fn fake_proc(name: &str) -> std::path::PathBuf {
        let root = crate::testing::track(std::env::temp_dir().join(format!("am-linux-proc-{name}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("net")).unwrap();
        let hdr = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let le = cfg!(target_endian = "little");
        // 127.0.0.1:5173 LISTEN、0.0.0.0:8080 LISTEN、127.0.0.1:9999 ESTABLISHED（不算）。
        let lo = if le { "0100007F" } else { "7F000001" };
        std::fs::write(
            root.join("net/tcp"),
            format!(
                "{hdr}   0: {lo}:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                    1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0 100 0 0 10 0\n\
                    2: {lo}:270F {lo}:D431 01 00000000:00000000 00:00000000 00000000  1000        0 333 1 0 20 4 30 10 -1\n"
            ),
        )
        .unwrap();
        // [::1]:3000 LISTEN、[::]:4000 LISTEN。
        let one = if le { "01000000" } else { "00000001" };
        std::fs::write(
            root.join("net/tcp6"),
            format!(
                "{hdr}   0: 000000000000000000000000{one}:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 444 1 0 100 0 0 10 0\n\
                    1: 00000000000000000000000000000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555 1 0 100 0 0 10 0\n"
            ),
        )
        .unwrap();
        let fds = |pid: i32, links: &[&str]| {
            let d = root.join(pid.to_string()).join("fd");
            std::fs::create_dir_all(&d).unwrap();
            for (i, l) in links.iter().enumerate() {
                symlink(l, d.join(i.to_string())).unwrap();
            }
        };
        fds(10, &["/dev/null", "socket:[111]", "socket:[333]", "pipe:[9]"]);
        fds(20, &["socket:[222]", "socket:[444]"]);
        fds(30, &["socket:[555]"]);
        fds(40, &["socket:[999]"]);
        symlink("/work/app", root.join("10/cwd")).unwrap();
        symlink("/work/other", root.join("20/cwd")).unwrap();
        root
    }

    #[test]
    fn listeners_come_out_in_lsof_fpn_format_and_parse_like_lsof() {
        let root = fake_proc("listen");
        let all = listen_fpn_in(&root, None);
        assert_eq!(all, "p10\nn127.0.0.1:5173\np20\nn*:8080\nn[::1]:3000\np30\nn*:4000\n", "ESTABLISHED 與沒 listen 的 pid 不列");
        let by_pid = crate::panes::parse_lsof(&all);
        assert_eq!(by_pid[&10], vec![5173]);
        assert_eq!(by_pid[&20], vec![3000, 8080]);
        assert!(!by_pid.contains_key(&40));
        // 預覽的綁定判斷：`*` 跟 `[::1]` 要分得出對外與 loopback。
        let l = crate::preview_bind::parse_listeners(&listen_fpn_in(&root, Some(&[20])));
        assert_eq!(l, vec![("*".to_string(), 8080), ("[::1]".to_string(), 3000)]);
        assert_eq!(crate::preview_bind::exposed_addr(&l), Some("*"));
        assert_eq!(listen_fpn_in(&root, Some(&[10, 99])), "p10\nn127.0.0.1:5173\n", "不存在的 pid 略過");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_comes_out_in_lsof_fpn_format() {
        let root = fake_proc("cwd");
        let out = cwd_fpn_in(&root, &[10, 20, 30]);
        assert_eq!(out, "p10\nn/work/app\np20\nn/work/other\n", "讀不到 cwd 的 pid 略過");
        let m = crate::preview::parse_lsof_cwd(&out);
        assert_eq!(m[&10], "/work/app");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_decodes_kernel_hex() {
        let le = cfg!(target_endian = "little");
        assert_eq!(endpoint(if le { "0100007F:0050" } else { "7F000001:0050" }, false).as_deref(), Some("127.0.0.1:80"));
        assert_eq!(endpoint(if le { "0101A8C0:1F90" } else { "C0A80101:1F90" }, false).as_deref(), Some("192.168.1.1:8080"));
        assert_eq!(endpoint("00000000:0016", false).as_deref(), Some("*:22"));
        assert_eq!(endpoint("0100007F", false), None, "沒有 port");
        assert_eq!(endpoint("XYZ:0050", false), None);
        assert_eq!(endpoint("0100007F:0050", true), None, "長度不對的 v6");
    }

    /// 真的正式路徑看自己剛 bind 的 port 與 cwd：Linux 驗核心的 `/proc` 格式（外部編譯主機），macOS 驗 `lsof`
    /// （`check.sh macos-local`）。Linux 的 `/proc` 一定讀得到；macOS 的 `lsof` 起不來或逾時就略過並印原因，比照 shell.rs 的煙霧測試。
    #[tokio::test]
    async fn macos_local_real_listeners_and_cwd_see_this_process() {
        let t = Duration::from_secs(10);
        let me = i32::try_from(std::process::id()).unwrap();
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let Some(out) = listen_fpn(Some(&[me]), t).await else {
            if cfg!(target_os = "linux") {
                panic!("Linux 的 /proc 一定讀得到");
            }
            return eprintln!("略過：`lsof` 起不來或逾時");
        };
        let l = crate::preview_bind::parse_listeners(&out);
        assert!(l.contains(&("127.0.0.1".to_string(), port)), "{out}");
        // 之後幾趟讀不到：Linux 算錯、macOS 照樣略過那一項。
        let read = |o: Option<String>, what: &str| {
            assert!(o.is_some() || !cfg!(target_os = "linux"), "Linux 的 /proc 讀不到：{what}");
            o
        };
        if let Some(out) = read(listen_fpn(None, t).await, "全機") {
            assert!(crate::panes::parse_lsof(&out).get(&me).is_some_and(|p| p.contains(&port)), "全機掃描也要看得到自己");
        }
        drop(v4);
        if let Some(out) = read(listen_fpn(Some(&[me]), t).await, "關掉之後") {
            assert!(!crate::preview_bind::parse_listeners(&out).iter().any(|(_, p)| *p == port), "關掉之後不再列：{out}");
        }
        if let Some(out) = read(cwd_fpn(&[me], t).await, "cwd") {
            let want = std::env::current_dir().unwrap().canonicalize().unwrap();
            let got = crate::preview::parse_lsof_cwd(&out).get(&me).map(|c| std::path::PathBuf::from(c).canonicalize().unwrap());
            assert_eq!(got, Some(want), "{out}");
        }
    }
