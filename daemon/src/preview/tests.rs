//! 預覽的測試。行程與 port 全走 [`FakeEnv`]，不碰真 herdr、真行程、真 port（#211）。

use super::*;
use crate::testing;
use std::sync::Mutex as StdMutex;

#[derive(Default)]
struct FakeEnv {
    spawned: StdMutex<Vec<(String, String, String)>>,
    alive: StdMutex<HashSet<String>>,
    listening: StdMutex<HashSet<u16>>,
    closed: StdMutex<Vec<String>>,
    tail: StdMutex<String>,
    next: std::sync::atomic::AtomicU32,
    spawn_fails: std::sync::atomic::AtomicBool,
    vites: StdMutex<Vec<ViteProc>>,
    /// 目錄 → 它的 repo；沒登記＝判不出來。
    repos: StdMutex<HashMap<String, RepoKey>>,
    /// pane → 它的行程樹實際 listen 的 `(位址, port)`（dev script 起的 server 自己挑的）。
    pane_listeners: StdMutex<HashMap<String, Vec<(String, u16)>>>,
    /// 模擬「拿不到這顆 bot 的 herdr session」（#257）。
    unresolvable: std::sync::atomic::AtomicBool,
    /// 模擬 herdr 的關 pane 失敗。
    close_fails: std::sync::atomic::AtomicBool,
    /// 模擬本機掃描失敗（問不到，不是「沒有」）。
    scan_fails: std::sync::atomic::AtomicBool,
    /// 模擬讀不到 dev script 行程樹的 listen 位址。
    pane_listeners_fails: std::sync::atomic::AtomicBool,
}

impl FakeEnv {
    fn listen(&self, port: u16) {
        self.listening.lock().unwrap().insert(port);
    }
    fn unlisten(&self, port: u16) {
        self.listening.lock().unwrap().remove(&port);
    }
    fn kill_pane(&self, pane: &str) {
        self.alive.lock().unwrap().remove(pane);
    }
    fn vite(&self, pid: i32, port: u16, cwd: &str) {
        self.server(pid, port, cwd, "vite");
    }
    fn server(&self, pid: i32, port: u16, cwd: &str, kind: &str) {
        self.vites.lock().unwrap().push(ViteProc { pid, port, cwd: cwd.into(), kind: kind.into() });
        self.listen(port);
    }
    /// 綁 loopback（正常情況）。
    fn pane_listens(&self, pane: &str, port: u16) {
        self.pane_listens_on(pane, crate::preview_bind::LOOPBACK, port);
    }
    /// 綁指定位址：`*` 之類的對外位址用來驗 #434 的檢查。
    fn pane_listens_on(&self, pane: &str, addr: &str, port: u16) {
        self.pane_listeners.lock().unwrap().entry(pane.into()).or_default().push((addr.into(), port));
        self.listen(port);
    }
    fn repo(&self, dir: &str, common: &str, origin: Option<&str>) {
        self.repos.lock().unwrap().insert(dir.into(), RepoKey { common: common.into(), origin: origin.map(Into::into) });
    }
    fn vite_exits(&self, port: u16) {
        self.vites.lock().unwrap().retain(|v| v.port != port);
        self.unlisten(port);
    }
    fn spawns(&self) -> Vec<(String, String, String)> {
        self.spawned.lock().unwrap().clone()
    }
}

impl PreviewEnv for FakeEnv {
    fn spawn<'a>(&'a self, target: &'a str, cwd: &'a str, cmd: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            if self.spawn_fails.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("herdr refused");
            }
            let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = format!("prev-{n}");
            self.spawned.lock().unwrap().push((target.into(), cwd.into(), cmd.into()));
            self.alive.lock().unwrap().insert(id.clone());
            Ok(id)
        })
    }
    fn pane_alive<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { Some(self.alive.lock().unwrap().contains(pane_id)) })
    }
    fn pane_tail<'a>(&'a self, _: &'a str) -> BoxFuture<'a, String> {
        Box::pin(async move { self.tail.lock().unwrap().clone() })
    }
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if self.close_fails.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.alive.lock().unwrap().remove(pane_id);
            self.closed.lock().unwrap().push(pane_id.into());
            true
        })
    }
    fn resolvable(&self) -> bool {
        !self.unresolvable.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.listening.lock().unwrap().contains(&port) })
    }
    fn scan_servers<'a>(&'a self, _roots: &'a [String]) -> BoxFuture<'a, Option<Vec<ViteProc>>> {
        Box::pin(async move {
            if self.scan_fails.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(self.vites.lock().unwrap().clone())
        })
    }
    fn pane_listeners<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<Vec<(String, u16)>>> {
        Box::pin(async move {
            if self.pane_listeners_fails.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(self.pane_listeners.lock().unwrap().get(pane_id).cloned().unwrap_or_default())
        })
    }
    fn repo_key<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Option<RepoKey>> {
        Box::pin(async move { self.repos.lock().unwrap().get(dir).cloned() })
    }
}

fn set(v: &[u16]) -> HashSet<u16> {
    v.iter().copied().collect()
}

// ── 純函式 ──

fn dirs(cwd: &str, files: &[&str], subs: &[(&str, &[&str])]) -> Result<Vec<PathBuf>, Vec<String>> {
    let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
    let subs: HashMap<PathBuf, Vec<PathBuf>> =
        subs.iter().map(|(d, v)| (PathBuf::from(d), v.iter().map(PathBuf::from).collect())).collect();
    detect_dirs(Path::new(cwd), |p| files.contains(p), |d| subs.get(d).cloned().unwrap_or_default(), |_| false)
}

#[test]
fn detect_lists_root_then_web_then_the_rest_by_depth_and_path() {
    let got = dirs(
        "/p",
        &["/p/vite.config.ts", "/p/web/vite.config.mts", "/p/apps/b/vite.config.js", "/p/apps/a/vite.config.mjs", "/p/packages/x/vite.config.ts"],
        &[("/p", &["/p/packages", "/p/web", "/p/apps"]), ("/p/apps", &["/p/apps/b", "/p/apps/a", "/p/apps/none"]), ("/p/packages", &["/p/packages/x"])],
    );
    // 根目錄自己有設定就不再往裡面找（它底下的是它的一部分）。
    assert_eq!(got.unwrap(), vec![PathBuf::from("/p")]);
    let got = dirs(
        "/p",
        &["/p/web/vite.config.mts", "/p/apps/b/vite.config.js", "/p/apps/a/vite.config.mjs", "/p/packages/x/vite.config.ts"],
        &[("/p", &["/p/packages", "/p/web", "/p/apps"]), ("/p/apps", &["/p/apps/b", "/p/apps/a", "/p/apps/none"]), ("/p/packages", &["/p/packages/x"])],
    )
    .unwrap();
    let want: Vec<PathBuf> = ["/p/web", "/p/apps/a", "/p/apps/b", "/p/packages/x"].iter().map(PathBuf::from).collect();
    assert_eq!(got, want);
}

#[test]
fn detect_finds_a_vite_three_levels_down_but_not_four() {
    // wits-ops：專案是 wt，vite 在 wt/webui/apps/web。
    let got = dirs(
        "/wt",
        &["/wt/webui/apps/web/vite.config.ts"],
        &[("/wt", &["/wt/webui"]), ("/wt/webui", &["/wt/webui/apps"]), ("/wt/webui/apps", &["/wt/webui/apps/web"])],
    );
    assert_eq!(got.unwrap(), vec![PathBuf::from("/wt/webui/apps/web")]);
    let deep = dirs(
        "/wt",
        &["/wt/a/b/c/web/vite.config.ts"],
        &[("/wt", &["/wt/a"]), ("/wt/a", &["/wt/a/b"]), ("/wt/a/b", &["/wt/a/b/c"]), ("/wt/a/b/c", &["/wt/a/b/c/web"])],
    );
    assert!(deep.is_err(), "第四層不找");
}

#[test]
fn detect_skips_dependency_build_hidden_and_worktree_dirs() {
    let got = dirs(
        "/p",
        &[
            "/p/node_modules/x/vite.config.ts",
            "/p/.git/hooks/vite.config.ts",
            "/p/.claude/worktrees/w/vite.config.ts",
            "/p/target/vite.config.ts",
            "/p/dist/vite.config.ts",
            "/p/worktrees/w/vite.config.ts",
            "/p/app/vite.config.ts",
        ],
        &[(
            "/p",
            &["/p/node_modules", "/p/.git", "/p/.claude", "/p/target", "/p/dist", "/p/worktrees", "/p/app"],
        ), ("/p/node_modules", &["/p/node_modules/x"]), ("/p/.git", &["/p/.git/hooks"]), ("/p/worktrees", &["/p/worktrees/w"])],
    );
    assert_eq!(got.unwrap(), vec![PathBuf::from("/p/app")]);
}

#[test]
fn detect_caps_candidates_and_directories_visited() {
    let names: Vec<String> = (0..30).map(|i| format!("/p/app{i:02}")).collect();
    let files: Vec<String> = names.iter().map(|n| format!("{n}/vite.config.ts")).collect();
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
    let kids: Vec<&str> = names.iter().map(String::as_str).collect();
    let got = dirs("/p", &files, &[("/p", &kids)]).unwrap();
    assert_eq!(got.len(), MAX_CANDIDATES);
    assert_eq!(got[0], PathBuf::from("/p/app00"), "排序後取前面的");
    // 走過的目錄數有上限：一個超寬的目錄不會掃到天荒地老。
    let wide: Vec<String> = (0..SEARCH_MAX_DIRS + 500).map(|i| format!("/p/d{i:05}")).collect();
    let wide_refs: Vec<&str> = wide.iter().map(String::as_str).collect();
    let last = format!("{}/vite.config.ts", wide.last().unwrap());
    assert!(dirs("/p", &[last.as_str()], &[("/p", &wide_refs)]).is_err(), "超過上限的那些沒被走到");
}

#[test]
fn detect_reports_what_it_tried() {
    let tried = dirs("/p", &[], &[]).unwrap_err();
    assert_eq!(tried.len(), 9);
    assert_eq!(tried[0], "/p/vite.config.ts");
    assert_eq!(tried[7], "/p/web/vite.config.mjs");
    assert!(tried[8].starts_with("/p/**/vite.config.*"), "{}", tried[8]);
}

#[test]
fn dev_servers_are_recognised_by_command_line() {
    for (cmd, kind) in [
        ("node /x/node_modules/.bin/vite --port 5173", "vite"),
        ("bun x vite --host 0.0.0.0", "vite"),
        ("node /x/node_modules/vite/bin/vite.js", "vite"),
        ("next-server (v16.3.5)", "next"),
        ("node /x/node_modules/.bin/next dev -p 3200", "next"),
        ("node /x/node_modules/.bin/webpack serve", "webpack"),
        ("node /x/webpack-dev-server --hot", "webpack"),
        ("node /x/.bin/astro dev", "astro"),
        ("node /x/.bin/remix-serve build", "remix"),
        ("node /x/.bin/storybook dev -p 6006", "storybook"),
        ("node /x/.bin/nuxi dev", "nuxt"),
        ("node /x/.bin/rsbuild dev", "rsbuild"),
        ("node /x/.bin/parcel src/index.html", "parcel"),
        ("node /x/.bin/ng serve", "angular"),
        ("node /x/.bin/react-scripts start", "react-scripts"),
        ("bun --hot server.ts", "bun"),
    ] {
        assert_eq!(dev_kind(cmd), Some(kind), "{cmd}");
    }
    for cmd in ["node /x/.bin/vitest run", "vim vite.config.ts", "/usr/bin/vitepress dev", "node /x/.bin/next build", "bun run build", "/usr/sbin/sshd -D", "ng build"] {
        assert_eq!(dev_kind(cmd), None, "{cmd}");
    }
}

#[test]
fn a_listener_counts_by_command_or_by_living_under_a_project() {
    let roots = vec!["/home/wt".to_string()];
    assert_eq!(classify_listener("next-server (v16)", "/elsewhere", &roots), Some("next"));
    assert_eq!(classify_listener("node server.js", "/home/wt/witsper-ops", &roots), Some("unknown"));
    assert_eq!(classify_listener("node server.js", "/home/wt", &roots), Some("unknown"));
    assert_eq!(classify_listener("node server.js", "/home/wt-other/x", &roots), None, "前綴相同不算底下");
    assert_eq!(classify_listener("node server.js", "/elsewhere", &roots), None);
    // 7788、ssh、資料庫不列，即使 cwd 在專案底下。
    for c in ["/x/target/release/agents-managerd serve", "ssh -N host", "/usr/local/bin/postgres -D data", "herdr server"] {
        assert_eq!(classify_listener(c, "/home/wt/agents-manager", &roots), None, "{c}");
    }
}

#[test]
fn join_servers_filters_and_labels_listeners() {
    let ports: HashMap<i32, Vec<u16>> = [(1, vec![3200]), (2, vec![7788]), (3, vec![22]), (4, vec![9999]), (5, vec![5173, 5174])].into();
    let cwds: HashMap<i32, String> = [
        (1, "/home/wt/witsper-ops".to_string()),
        (2, "/home/wt/agents-manager".to_string()),
        (3, "/home/wt".to_string()),
        (4, "/unrelated".to_string()),
        (5, "/unrelated/web".to_string()),
    ]
    .into();
    let cmds: HashMap<i32, String> = [
        (1, "next-server (v16.3.5)".to_string()),
        (2, "/x/agents-managerd serve".to_string()),
        (3, "sshd: me".to_string()),
        (4, "node other.js".to_string()),
        (5, "node /x/.bin/vite".to_string()),
    ]
    .into();
    let got = join_servers(&ports, &cwds, &cmds, &["/home/wt".to_string()], 999);
    let brief: Vec<(u16, &str)> = got.iter().map(|p| (p.port, p.kind.as_str())).collect();
    assert_eq!(brief, vec![(3200, "next"), (5173, "vite"), (5174, "vite")]);
    // 自己的 pid 不列。
    assert!(join_servers(&ports, &cwds, &cmds, &["/home/wt".to_string()], 1).iter().all(|p| p.port != 3200));
}

#[test]
fn package_json_dev_script_and_the_command_that_follows() {
    assert_eq!(parse_dev_script(r#"{"scripts":{"dev":"next dev -p 3200","build":"x"}}"#).as_deref(), Some("next dev -p 3200"));
    assert_eq!(parse_dev_script(r#"{"scripts":{"build":"x"}}"#), None);
    assert_eq!(parse_dev_script(r#"{"scripts":{"dev":"  "}}"#), None);
    assert_eq!(parse_dev_script("not json"), None);
    // #434：`allow_lan` 關著時認得出的框架要接上 loopback 旗標；開著就照專案自己的意思。
    assert_eq!(run_command(Some("next dev"), "next", false, 5180), "bun run dev -- -H 127.0.0.1");
    assert_eq!(run_command(Some("vite"), "vite", false, 5180), "bun run dev -- --host 127.0.0.1");
    assert_eq!(run_command(Some("node server.js"), "unknown", false, 5180), "bun run dev", "認不出框架就別猜旗標");
    assert_eq!(run_command(Some("next dev"), "next", true, 5180), "bun run dev", "allow_lan 開著不插手");
    assert_eq!(run_command(None, "vite", false, 5181), "bunx vite --host 127.0.0.1 --port 5181 --strictPort");
}

#[test]
fn classify_attaches_only_to_the_bots_own_checkout() {
    let procs = vec![
        ViteProc { pid: 1, port: 5173, cwd: "/main/web".into(), kind: "vite".into() },
        ViteProc { pid: 2, port: 5241, cwd: "/mine/web/".into(), kind: "vite".into() },
        ViteProc { pid: 3, port: 3001, cwd: "/other/apps/web".into(), kind: "vite".into() },
    ];
    let cands = vec![PathBuf::from("/mine"), PathBuf::from("/mine/web")];
    let (hit, others) = classify(&procs, &cands);
    assert_eq!(hit.unwrap().port, 5241, "尾端斜線不影響比對");
    assert_eq!(others.iter().map(|p| p.port).collect::<Vec<_>>(), vec![5173, 3001]);
    let (none, all) = classify(&procs, &[PathBuf::from("/mine/apps/x")]);
    assert!(none.is_none());
    assert_eq!(all.len(), 3);
}

#[test]
fn pick_port_starts_at_5180_and_skips_taken_and_listening() {
    assert_eq!(pick_port(&set(&[]), &set(&[])), Some(5180));
    assert_eq!(pick_port(&set(&[5180]), &set(&[5181])), Some(5182));
    let all: HashSet<u16> = (PORT_START..PORT_START + PORT_SPAN).collect();
    assert_eq!(pick_port(&all, &set(&[])), None);
    // 5173 是人手開的，永遠不在窗口內。
    assert!(pick_port(&set(&[]), &set(&[])).unwrap() > 5173);
}

#[test]
fn command_binds_by_allow_lan() {
    assert_eq!(command(true, 5181), "bunx vite --host 0.0.0.0 --port 5181 --strictPort");
    assert_eq!(command(false, 5180), "bunx vite --host 127.0.0.1 --port 5180 --strictPort");
}

#[test]
fn transitions() {
    let o = |alive, listening| Observed { pane_alive: alive, listening };
    use Status::*;
    assert_eq!(next_status(Starting, false, o(Some(true), true), 3), Next::To(Running, None));
    assert_eq!(next_status(Starting, false, o(Some(true), false), 3), Next::Stay);
    assert_eq!(next_status(Starting, false, o(None, false), 3), Next::Stay);
    assert!(matches!(next_status(Starting, false, o(Some(true), false), 60), Next::To(Failed, Some(_))));
    assert!(matches!(next_status(Starting, false, o(Some(false), false), 1), Next::To(Failed, Some(_))));
    // port 已經起來的那一拍，pane 讀不到不該壓過它。
    assert_eq!(next_status(Starting, false, o(Some(false), true), 1), Next::To(Running, None));
    assert_eq!(next_status(Running, false, o(Some(true), true), 999), Next::Stay);
    assert_eq!(next_status(Running, false, o(Some(false), true), 1), Next::To(Off, None));
    assert!(matches!(next_status(Running, false, o(Some(true), false), 1), Next::To(Failed, Some(_))));
    assert_eq!(next_status(Running, false, o(None, true), 1), Next::Stay);
    assert_eq!(next_status(Off, false, o(Some(true), true), 1), Next::Stay);
    assert_eq!(next_status(Failed, false, o(Some(true), true), 1), Next::Stay);
    // 接上的：port 不見就是斷開（off），不是失敗；它沒有 pane，pane 那一欄不參與。
    assert_eq!(next_status(Running, true, o(None, false), 1), Next::To(Off, None));
    assert_eq!(next_status(Running, true, o(None, true), 1), Next::Stay);
}

// ── DB＋假 env ──

struct Rig {
    e: testing::Env,
    fake: Arc<FakeEnv>,
}

async fn rig() -> Rig {
    let e = testing::env().await;
    let fake = Arc::new(FakeEnv::default());
    *e.app.preview_env.lock().unwrap() = Some(fake.clone());
    std::fs::create_dir_all(e.repo.join("web")).unwrap();
    std::fs::write(e.repo.join("web/vite.config.ts"), "export default {}").unwrap();
    Rig { e, fake }
}

async fn running_bot(r: &Rig, name: &str) -> String {
    let b = testing::claude_bot(&r.e.app, &r.e.project_id, name).await;
    testing::fake_run(&r.e.app, &b.id).await;
    b.id
}

fn status(v: &Value) -> &str {
    v["status"].as_str().unwrap()
}

#[tokio::test]
async fn start_opens_a_pane_next_to_the_bot_in_the_detected_dir() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(status(&body), "starting");
    assert_eq!(body["port"], 5180);
    let dir = r.e.repo.join("web").to_string_lossy().into_owned();
    assert_eq!(body["dir"], dir.as_str());
    let spawns = r.fake.spawns();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].0, format!("pane-{bot}"), "切在 bot 自己的 pane 旁邊（同一個 tab）");
    assert_eq!(spawns[0].1, dir);
    assert_eq!(spawns[0].2, "bunx vite --host 127.0.0.1 --port 5180 --strictPort");
}

#[tokio::test]
async fn start_does_not_spawn_through_the_fallback_when_the_owner_session_is_unreadable() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.unresolvable.store(true, SeqCst);

    assert!(start(&r.e.app, &bot, StartReq::default()).await.is_err(), "拿不到 owner session 不能假裝成功");
    assert!(r.fake.spawns().is_empty(), "不能退回管理 session 在錯的 pane 起 server");
    assert!(row(&r.e.app.db, &bot).await.unwrap().is_none(), "失敗不能留下半套 preview row");
}

#[tokio::test]
async fn start_is_idempotent_while_starting_or_running() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let first = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let again = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(first["pane_id"], again["pane_id"]);
    r.fake.listen(5180);
    let running = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(status(&running), "running");
    assert_eq!(r.fake.spawns().len(), 1, "只開過一顆 pane");
}

#[tokio::test]
async fn an_explicit_restart_does_not_spawn_when_the_existing_preview_cannot_be_closed() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.close_fails.store(true, SeqCst);

    let err = start(&r.e.app, &bot, StartReq { mode: Some("spawn".into()), ..StartReq::default() }).await.unwrap_err();
    let LcError::Conflict(body) = err else { panic!("關不掉舊預覽時要拒絕換開：{err:?}") };
    assert_eq!(body["reason"], "preview_stop_failed");
    assert_eq!(r.fake.spawns().len(), 1, "不能在舊 pane 還活著時再開一顆");
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
}

#[tokio::test]
async fn two_bots_get_different_ports() {
    let r = rig().await;
    let a = running_bot(&r, "alfa").await;
    let b = running_bot(&r, "bravo").await;
    let pa = start(&r.e.app, &a, StartReq::default()).await.unwrap();
    let pb = start(&r.e.app, &b, StartReq::default()).await.unwrap();
    assert_eq!(pa["port"], 5180);
    assert_eq!(pb["port"], 5181);
}

#[tokio::test]
async fn a_port_someone_else_is_listening_on_is_skipped() {
    let r = rig().await;
    r.fake.listen(5180);
    let bot = running_bot(&r, "alfa").await;
    assert_eq!(start(&r.e.app, &bot, StartReq::default()).await.unwrap()["port"], 5181);
}

#[tokio::test]
async fn get_advances_starting_to_running_and_emits() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let seq = r.e.app.current_seq();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "starting");
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert!(r.e.app.current_seq() > seq, "preview_changed 有發出去");
    let map = state_map(&r.e.app.db).await.unwrap();
    assert_eq!(map[&bot], json!({"status": "running", "port": 5180, "source": "spawned"}));
}

/// #527：`allow_lan` 關著時我們起的 dev server 釘在 loopback（#434／#452），從手機或別台機器開的
/// 前端連不到它。三個端點都要帶這一格，前端才知道要把 iframe 換成說明，而不是給一片空白。
#[tokio::test]
async fn every_preview_body_says_whether_the_dev_server_is_reachable_from_elsewhere() {
    let mut r = rig().await;
    assert!(!r.e.app.allow_lan, "測試的 App 預設 allow_lan 關著");
    let bot = running_bot(&r, "alfa").await;
    // 面板進來的第一次 GET 就是 off，那一份也要有。
    assert_eq!(get(&r.e.app, &bot).await.unwrap()["lan"], json!(false));
    assert_eq!(start(&r.e.app, &bot, StartReq::default()).await.unwrap()["lan"], json!(false));
    r.fake.listen(5180);
    let running = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&running), "running");
    assert_eq!(running["lan"], json!(false));
    // DELETE 不走 `decorated`，自己補的那一份也要有。
    assert_eq!(stop(&r.e.app, &bot).await.unwrap()["lan"], json!(false));

    Arc::get_mut(&mut r.e.app).expect("no other handle").allow_lan = true;
    assert_eq!(get(&r.e.app, &bot).await.unwrap()["lan"], json!(true), "開著就是 true，不是寫死的");
}

#[tokio::test]
async fn never_started_is_off_and_absent_from_state() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&off), "off");
    assert_eq!(off["candidates"], json!([r.e.repo.join("web").to_string_lossy()]));
    assert_eq!(off["others"], json!([]));
    assert!(state_map(&r.e.app.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_start_that_never_listens_fails_with_the_pane_tail() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    *r.fake.tail.lock().unwrap() = "error: port 5180 is in use\n".into();
    let old = (chrono::Utc::now() - chrono::Duration::seconds(61)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE bot_previews SET started_at = ? WHERE bot_id = ?").bind(old).bind(&bot).execute(&r.e.app.db).await.unwrap();
    let body = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&body), "failed");
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("port 5180 is in use"), "{err}");
    // failed 不再佔 port：下一次啟動拿回 5180。
    assert_eq!(start(&r.e.app, &bot, StartReq::default()).await.unwrap()["port"], 5180);
}

#[tokio::test]
async fn retry_closes_a_failed_preview_pane_before_spawning_again() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let first = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let old_pane = first["pane_id"].as_str().unwrap().to_string();
    let old = (chrono::Utc::now() - chrono::Duration::seconds(61)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE bot_previews SET started_at = ? WHERE bot_id = ?").bind(old).bind(&bot).execute(&r.e.app.db).await.unwrap();

    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "failed");
    assert!(r.fake.closed.lock().unwrap().is_empty(), "失敗當下仍保留 pane，應由重試清理");

    let second = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![old_pane]);
    assert_ne!(second["pane_id"], first["pane_id"]);
}

#[tokio::test]
async fn a_running_preview_whose_vite_died_fails_and_one_whose_pane_was_closed_goes_off() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    r.fake.unlisten(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "failed");

    let again = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let pane = again["pane_id"].as_str().unwrap().to_string();
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    r.fake.kill_pane(&pane);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
}

#[tokio::test]
async fn stop_closes_the_pane_and_frees_the_port() {
    let r = rig().await;
    let a = running_bot(&r, "alfa").await;
    let b = running_bot(&r, "bravo").await;
    let started = start(&r.e.app, &a, StartReq::default()).await.unwrap();
    let pane = started["pane_id"].as_str().unwrap().to_string();
    assert_eq!(stop(&r.e.app, &a).await.unwrap(), json!({"status": "off", "lan": false}));
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(status(&get(&r.e.app, &a).await.unwrap()), "off");
    assert!(state_map(&r.e.app.db).await.unwrap().is_empty());
    assert_eq!(start(&r.e.app, &b, StartReq::default()).await.unwrap()["port"], 5180, "5180 放出來了");
    // 沒開過的 bot 停也是 off，不動任何 pane。
    let c = running_bot(&r, "charlie").await;
    assert_eq!(stop(&r.e.app, &c).await.unwrap(), json!({"status": "off", "lan": false}));
    assert_eq!(r.fake.closed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stop_reports_failure_when_the_preview_pane_cannot_be_closed() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.close_fails.store(true, SeqCst);

    let LcError::Conflict(body) = stop(&r.e.app, &bot).await.unwrap_err() else { panic!("關不掉 pane 時 DELETE 要失敗") };
    assert_eq!(body["reason"], "preview_stop_failed");
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stopping_the_bot_takes_its_preview_with_it() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    stop_for_bot(&r.e.app, &bot).await;
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
}

#[tokio::test]
async fn a_preview_whose_bot_no_longer_runs_is_reaped_on_the_next_look() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE runs SET state = 'exited' WHERE bot_id = ?").bind(&bot).execute(&r.e.app.db).await.unwrap();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
}

#[tokio::test]
async fn only_top_level_user_bots_may_preview() {
    let r = rig().await;
    let parent = running_bot(&r, "alfa").await;
    let child = running_bot(&r, "alfa-c").await;
    sqlx::query("UPDATE bots SET managed_by = 'child', parent_bot_id = ? WHERE id = ?")
        .bind(&parent)
        .bind(&child)
        .execute(&r.e.app.db)
        .await
        .unwrap();
    let e = start(&r.e.app, &child, StartReq::default()).await.unwrap_err();
    let LcError::Conflict(v) = e else { panic!("{e:?}") };
    assert_eq!(v["reason"], "not_top_level");
    assert!(r.fake.spawns().is_empty());
}

#[tokio::test]
async fn no_vite_config_lists_what_was_tried() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let bot = running_bot(&r, "alfa").await;
    let LcError::Conflict(v) = start(&r.e.app, &bot, StartReq::default()).await.unwrap_err() else { panic!() };
    assert_eq!(v["reason"], "no_vite_config");
    assert_eq!(v["tried"].as_array().unwrap().len(), 9);
    assert!(r.fake.spawns().is_empty());
}

#[tokio::test]
async fn a_bot_cwd_overrides_the_project_path_and_a_stopped_bot_cannot_preview() {
    let r = rig().await;
    let elsewhere = r.e.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("vite.config.js"), "").unwrap();
    let bot = running_bot(&r, "alfa").await;
    sqlx::query("UPDATE bots SET cwd = ? WHERE id = ?").bind(elsewhere.to_string_lossy().into_owned()).bind(&bot).execute(&r.e.app.db).await.unwrap();
    assert_eq!(start(&r.e.app, &bot, StartReq::default()).await.unwrap()["dir"], elsewhere.to_string_lossy().as_ref());

    let idle = testing::claude_bot(&r.e.app, &r.e.project_id, "bravo").await;
    let LcError::Conflict(v) = start(&r.e.app, &idle.id, StartReq::default()).await.unwrap_err() else { panic!() };
    assert_eq!(v["reason"], "bot_not_running");
}

#[tokio::test]
async fn a_refused_split_leaves_no_row_behind() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.spawn_fails.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(start(&r.e.app, &bot, StartReq::default()).await.is_err());
    assert!(row(&r.e.app.db, &bot).await.unwrap().is_none());
}

#[tokio::test]
async fn the_lan_flag_widens_the_bind() {
    let mut r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    Arc::get_mut(&mut r.e.app).expect("no other handle").allow_lan = true;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert!(r.fake.spawns()[0].2.contains("--host 0.0.0.0"));
}

#[tokio::test]
async fn startup_reconcile_matches_rows_against_panes_and_ports() {
    let r = rig().await;
    let a = running_bot(&r, "alfa").await;
    let b = running_bot(&r, "bravo").await;
    let c = running_bot(&r, "charlie").await;
    let pa = start(&r.e.app, &a, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    start(&r.e.app, &b, StartReq::default()).await.unwrap();
    start(&r.e.app, &c, StartReq::default()).await.unwrap();
    r.fake.listen(5180);
    r.fake.listen(5181);
    assert_eq!(status(&get(&r.e.app, &a).await.unwrap()), "running");
    assert_eq!(status(&get(&r.e.app, &b).await.unwrap()), "running");
    // 「重啟」：同一個 DB、同一個假 env，記憶體歸零；期間 a 的 pane 被關、b 的 vite 死了、c 還在 starting 就 listen。
    let app2 = testing::restart_app(&r.e).await;
    *app2.preview_env.lock().unwrap() = Some(r.fake.clone());
    r.fake.kill_pane(&pa);
    r.fake.unlisten(5181);
    r.fake.listen(5182);
    reconcile_all(&app2).await;
    assert_eq!(row(&app2.db, &a).await.unwrap().unwrap().status, "off");
    assert_eq!(row(&app2.db, &b).await.unwrap().unwrap().status, "failed");
    assert_eq!(row(&app2.db, &c).await.unwrap().unwrap().status, "running");
}

/// 讀不到 runs（改表名），bot_previews 仍讀得到：模擬 active_run 的暫時性 DB 故障（#299）。
async fn break_runs(r: &Rig) {
    sqlx::query("ALTER TABLE runs RENAME TO runs_broken").execute(&r.e.app.db).await.unwrap();
}
async fn fix_runs(r: &Rig) {
    sqlx::query("ALTER TABLE runs_broken RENAME TO runs").execute(&r.e.app.db).await.unwrap();
}

#[tokio::test]
async fn an_unreadable_active_run_never_closes_a_healthy_preview() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    break_runs(&r).await;
    // GET／開機對帳都會走 refresh_locked：讀不到 run 是「不知道」，不是「bot 停了」。
    get(&r.e.app, &bot).await.unwrap();
    reconcile_all(&r.e.app).await;
    assert!(r.fake.closed.lock().unwrap().is_empty(), "健康的 vite pane 被關了");
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "running");
    // 讀得回來之後照常運作。
    fix_runs(&r).await;
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stopping_with_an_unreadable_run_keeps_the_row_until_the_owner_session_is_known() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    break_runs(&r).await;
    stop_for_bot(&r.e.app, &bot).await;
    // 不知道該用哪個 session 關：不關、也不寫 off（否則 pane 與 port 變成沒人管的孤兒）。
    assert!(r.fake.closed.lock().unwrap().is_empty());
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
    fix_runs(&r).await;
    stop_for_bot(&r.e.app, &bot).await;
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "off");
}

#[tokio::test]
async fn cleanup_never_goes_through_another_session_when_the_owner_session_is_unavailable() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    // 有 run，但拿不到它那個 session 的 client：不可退回管理 session 去關。
    r.fake.unresolvable.store(true, SeqCst);
    stop_for_bot(&r.e.app, &bot).await;
    assert!(r.fake.closed.lock().unwrap().is_empty());
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
    // bot 已經沒有 run 的那一條：同樣要靠 bot 自己的 session，拿不到就不動。
    sqlx::query("UPDATE runs SET state = 'exited' WHERE bot_id = ?").bind(&bot).execute(&r.e.app.db).await.unwrap();
    get(&r.e.app, &bot).await.unwrap();
    assert!(r.fake.closed.lock().unwrap().is_empty());
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
    // session 回來：在那個 session 關掉 pane，這時才記 off。
    r.fake.unresolvable.store(false, SeqCst);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
}

#[tokio::test]
async fn a_close_that_fails_or_is_unconfirmed_does_not_record_off() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    r.fake.close_fails.store(true, SeqCst);
    stop_for_bot(&r.e.app, &bot).await;
    // pane 與 port 還在：不能寫成 off 讓 vite 變成沒人管的孤兒。
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "starting");
    r.fake.close_fails.store(false, SeqCst);
    stop_for_bot(&r.e.app, &bot).await;
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "off");
}

#[tokio::test]
async fn the_starting_watcher_survives_a_herdr_session_that_is_briefly_unavailable() {
    use std::sync::atomic::Ordering::SeqCst;
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.unresolvable.store(true, SeqCst);
    spawn_watcher(r.e.app.clone(), bot.clone());
    // 好幾拍都拿不到 client：watcher 不能因此結束。
    tokio::time::sleep(Duration::from_millis(400)).await;
    r.fake.unresolvable.store(false, SeqCst);
    r.fake.listen(5180);
    assert!(
        testing::eventually!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "running"),
        "watcher 在 herdr 暫時拿不到時就結束了，沒人把 starting 推到 running"
    );
}

#[tokio::test]
async fn the_starting_watcher_survives_an_unreadable_preview_row() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    // 用 testing 的注入（先刷新同一條連線的 schema 快取再 ALTER）：裸 ALTER 會隨機抽到沒看過上一次改動的連線而 `no such table`。
    testing::make_table_unreadable(&r.e.app, "bot_previews").await;
    let before = watcher_ticks(&bot);
    assert!(spawn_watcher(r.e.app.clone(), bot.clone()));
    // 等它在讀不到的期間**真的輪過幾次**（事件），不是睡一段固定時間就當作有輪過。
    assert!(testing::eventually!(watcher_ticks(&bot) >= before + 3), "watcher 沒有在讀不到的期間輪過");
    assert!(is_watched(&bot), "讀不到那一列（暫時性）時 watcher 就結束了");
    testing::make_table_readable(&r.e.app, "bot_previews").await;
    r.fake.listen(5180);
    assert!(
        testing::eventually!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "running"),
        "恢復之後 watcher 要接著把它推到 running"
    );
}

#[tokio::test]
async fn the_watcher_promotes_a_starting_preview_without_anyone_asking() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    spawn_watcher(r.e.app.clone(), bot.clone());
    r.fake.listen(5180);
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "running" {
            return;
        }
    }
    panic!("the watcher never noticed the port");
}

// ── #260：running 之後也有人看 ──

#[tokio::test]
async fn a_spawned_preview_whose_server_dies_after_running_fails_without_anyone_asking() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.listen(5180);
    assert!(spawn_watcher(r.e.app.clone(), bot.clone()));
    assert!(testing::eventually!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "running"));
    // starting → running 之後監看還在（以前這裡就結束了）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(is_watched(&bot), "監看在 running 之後就結束了");
    let seq = r.e.app.current_seq();
    r.fake.unlisten(5180);
    assert!(
        testing::eventually!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "failed"),
        "vite 掛了（pane 還在、port 不再 listen），沒人 GET 就沒人發現"
    );
    assert!(r.e.app.current_seq() > seq, "preview_changed 有發出去");
    // failed 是終點：監看結束，之後重試（POST）才會再掛一個。
    assert!(testing::eventually!(!is_watched(&bot)));
}

#[tokio::test]
async fn an_attached_preview_goes_off_by_itself_when_the_external_server_exits() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.vite(100, 5180, &web_dir(&r));
    start(&r.e.app, &bot, req("attach", Some(5180), None)).await.unwrap();
    assert!(spawn_watcher(r.e.app.clone(), bot.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status, "running");
    r.fake.vite_exits(5180);
    assert!(
        testing::eventually!(row(&r.e.app.db, &bot).await.unwrap().unwrap().status == "off"),
        "接上的 vite 結束了，沒人 GET 就沒人發現"
    );
    // 別人的 server：只斷開，沒有動任何 pane。
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn there_is_only_one_monitor_per_bot_and_a_finished_one_makes_room() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert!(spawn_watcher(r.e.app.clone(), bot.clone()));
    // 重複的 GET／POST／開機對帳都會呼叫：不會再多一個。
    assert!(!spawn_watcher(r.e.app.clone(), bot.clone()));
    reconcile_all(&r.e.app).await;
    assert!(!spawn_watcher(r.e.app.clone(), bot.clone()));
    // 收掉預覽之後監看結束並註銷；下一顆預覽能重新掛。
    stop_for_bot(&r.e.app, &bot).await;
    assert!(testing::eventually!(!is_watched(&bot)));
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert!(spawn_watcher(r.e.app.clone(), bot.clone()));
}

#[tokio::test]
async fn startup_reconcile_also_monitors_a_preview_that_was_already_running() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    // 「重啟」：記憶體歸零，DB 裡那一列是 running；重啟後掛回監看，之後掛掉照樣發現。
    let app2 = testing::restart_app(&r.e).await;
    *app2.preview_env.lock().unwrap() = Some(r.fake.clone());
    reconcile_all(&app2).await;
    assert!(is_watched(&bot), "重啟後 running 的預覽沒人看");
    r.fake.unlisten(5180);
    assert!(testing::eventually!(row(&app2.db, &bot).await.unwrap().unwrap().status == "failed"));
}

// ── #253 v2：接上既有的 vite ──

fn web_dir(r: &Rig) -> String {
    r.e.repo.join("web").to_string_lossy().into_owned()
}

fn req(mode: &str, port: Option<u16>, dir: Option<&str>) -> StartReq {
    StartReq { mode: Some(mode.into()), port, dir: dir.map(Into::into), pid: None }
}

#[tokio::test]
async fn auto_attaches_to_a_vite_already_running_in_the_same_checkout() {
    let r = rig().await;
    r.fake.vite(4242, 5241, &web_dir(&r));
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(status(&body), "running");
    assert_eq!(body["source"], "attached");
    assert_eq!(body["port"], 5241);
    assert_eq!(body["pid"], 4242);
    assert_eq!(body["pane_id"], Value::Null);
    assert!(r.fake.spawns().is_empty(), "接上就不另起");
    let state = state_map(&r.e.app.db).await.unwrap();
    assert_eq!(state[&bot], json!({"status": "running", "port": 5241, "source": "attached"}));
}

#[tokio::test]
async fn another_checkout_is_listed_not_attached_and_a_spawn_follows() {
    let r = rig().await;
    r.fake.vite(1, 5173, "/somewhere/agents-manager-main/web");
    r.fake.repo("/somewhere/agents-manager-main/web", "/git/am/.git", None);
    r.fake.repo(&r.e.repo.to_string_lossy(), "/git/am/.git", None);
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(body["source"], "spawned");
    assert_eq!(r.fake.spawns().len(), 1);
    assert_eq!(body["port"], 5180);
    // 起好之後停掉，回 off 的畫面要列出別份 checkout 讓使用者選。
    stop(&r.e.app, &bot).await.unwrap();
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(
        off["others"],
        json!([{"port": 5173, "dir": "/somewhere/agents-manager-main/web", "pid": 1, "kind": "vite", "relation": "same_repo", "repo": "am"}])
    );
}

#[tokio::test]
async fn disconnecting_an_attached_preview_never_touches_the_others_server() {
    let r = rig().await;
    r.fake.vite(4242, 5241, &web_dir(&r));
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(stop(&r.e.app, &bot).await.unwrap(), json!({"status": "off", "lan": false}));
    assert!(r.fake.closed.lock().unwrap().is_empty(), "沒有關任何 pane");
    assert!(r.fake.vites.lock().unwrap().iter().any(|v| v.pid == 4242), "vite 還活著");
    // bot 被停也一樣：只斷開。
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    stop_for_bot(&r.e.app, &bot).await;
    assert!(r.fake.closed.lock().unwrap().is_empty());
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
}

#[tokio::test]
async fn an_attached_preview_goes_off_when_that_server_exits_even_without_a_running_bot() {
    let r = rig().await;
    r.fake.vite(4242, 5241, &web_dir(&r));
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    sqlx::query("UPDATE runs SET state = 'exited' WHERE bot_id = ?").bind(&bot).execute(&r.e.app.db).await.unwrap();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running", "接上的只看它自己的 port");
    r.fake.vite_exits(5241);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn attach_mode_needs_a_port_that_really_is_a_vite() {
    let r = rig().await;
    r.fake.listen(9000); // 有人在 listen，但不是 vite
    r.fake.vite(7, 3001, "/somewhere/hermes/apps/web");
    let bot = running_bot(&r, "alfa").await;
    let LcError::Bad(_) = start(&r.e.app, &bot, req("attach", None, None)).await.unwrap_err() else { panic!() };
    let LcError::Conflict(v) = start(&r.e.app, &bot, req("attach", Some(9000), None)).await.unwrap_err() else { panic!() };
    assert_eq!(v["reason"], "not_vite");
    // 使用者明確選了別份 checkout 的：接。
    let body = start(&r.e.app, &bot, req("attach", Some(3001), None)).await.unwrap();
    assert_eq!((body["source"].as_str(), body["dir"].as_str()), (Some("attached"), Some("/somewhere/hermes/apps/web")));
}

// ── #258：接上的預覽綁的是那顆行程，不只是 port ──

#[tokio::test]
async fn an_attached_preview_does_not_silently_follow_an_unrelated_process_that_reused_the_port() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.vite(100, 5180, &web_dir(&r));
    start(&r.e.app, &bot, req("attach", Some(5180), None)).await.unwrap();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    // 原本那顆結束、別的行程佔了同一個 port（別份 checkout 的 vite／不相干的本機服務）。
    r.fake.vite_exits(5180);
    r.fake.server(200, 5180, "/other/web", "vite");
    let seq = r.e.app.current_seq();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
    assert!(r.e.app.current_seq() > seq, "身分變了要推 preview_changed");
    // 不相干的服務（掃不到是 dev server）也一樣。
    r.fake.vite_exits(5180);
    r.fake.vite(300, 5180, &web_dir(&r));
    start(&r.e.app, &bot, req("attach", Some(5180), None)).await.unwrap();
    r.fake.vite_exits(5180);
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
    assert!(r.fake.closed.lock().unwrap().is_empty(), "別人的 server 一律不動");
}

#[tokio::test]
async fn an_attached_preview_follows_the_same_app_restarting_with_a_new_pid() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.vite(100, 5180, &web_dir(&r));
    start(&r.e.app, &bot, req("attach", Some(5180), None)).await.unwrap();
    // 同一個目錄的 vite 重啟（pid 換了、port 沒變）：仍是那個 app，沿用並記下新 pid。
    r.fake.vite_exits(5180);
    r.fake.vite(101, 5180, &web_dir(&r));
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert_eq!(row(&r.e.app.db, &bot).await.unwrap().unwrap().pid, Some(101));
    // 掃描失敗（問不到）不當成換人；掃得到而且 port 上沒有 dev server 才是換人了。
    r.fake.scan_fails.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    r.fake.scan_fails.store(false, std::sync::atomic::Ordering::SeqCst);
    r.fake.vites.lock().unwrap().clear();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
}

#[tokio::test]
async fn a_stale_attach_selection_fails_instead_of_attaching_whatever_took_the_port() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.vite(100, 5180, &web_dir(&r));
    // GET 看到 pid 100；POST 之前它結束、別份 checkout 的 vite 佔了 5180。
    r.fake.vite_exits(5180);
    r.fake.server(200, 5180, "/other/web", "vite");
    let stale = StartReq { pid: Some(100), ..req("attach", Some(5180), Some(&web_dir(&r))) };
    let LcError::Conflict(v) = start(&r.e.app, &bot, stale).await.unwrap_err() else { panic!("要 409") };
    assert_eq!(v["reason"], "stale_selection");
    assert!(row(&r.e.app.db, &bot).await.unwrap().is_none(), "不可留下接到別人的那一列");
    // 只帶 dir 也對得上才接；pid／dir 都對就接。
    let dir_only = req("attach", Some(5180), Some(&web_dir(&r)));
    assert!(matches!(start(&r.e.app, &bot, dir_only).await.unwrap_err(), LcError::Conflict(_)));
    let good = StartReq { pid: Some(200), ..req("attach", Some(5180), Some("/other/web")) };
    assert_eq!(start(&r.e.app, &bot, good).await.unwrap()["pid"], 200);
    // 同一個目錄、只是換了一顆行程：只帶 pid 也要擋（那是另一顆 server 了）。
    r.fake.vite_exits(5180);
    r.fake.vite(101, 5180, "/other/web");
    let pid_only = StartReq { pid: Some(200), ..req("attach", Some(5180), None) };
    let LcError::Conflict(v) = start(&r.e.app, &bot, pid_only).await.unwrap_err() else { panic!("要 409") };
    assert_eq!(v["reason"], "stale_selection");
}

#[tokio::test]
async fn spawn_mode_and_dir_choice_override_auto_and_replace_a_live_preview() {
    let r = rig().await;
    std::fs::create_dir_all(r.e.repo.join("apps/site")).unwrap();
    std::fs::write(r.e.repo.join("apps/site/vite.config.ts"), "").unwrap();
    r.fake.vite(4242, 5241, &web_dir(&r));
    let bot = running_bot(&r, "alfa").await;
    let first = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(first["source"], "attached");
    assert_eq!(first["candidates"].as_array().unwrap().len(), 2);
    let site = r.e.repo.join("apps/site").to_string_lossy().into_owned();
    // 換成自己起、而且挑另一個目錄：舊的（接上的）只斷開。
    let second = start(&r.e.app, &bot, req("spawn", None, Some(&site))).await.unwrap();
    assert_eq!(second["source"], "spawned");
    assert_eq!(second["dir"], site.as_str());
    assert!(r.fake.closed.lock().unwrap().is_empty());
    assert!(r.fake.vites.lock().unwrap().iter().any(|v| v.pid == 4242));
    let LcError::Bad(_) = start(&r.e.app, &bot, req("spawn", None, Some("/not/a/candidate"))).await.unwrap_err() else { panic!() };
    let LcError::Bad(_) = start(&r.e.app, &bot, req("bogus", None, None)).await.unwrap_err() else { panic!() };
}

#[tokio::test]
async fn a_spawned_preview_is_still_closed_by_stop() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    stop(&r.e.app, &bot).await.unwrap();
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
}

#[tokio::test]
async fn opening_a_pre_v2_database_adds_the_source_and_pid_columns() {
    let dir = std::env::temp_dir().join(format!("am-test-{}", db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite3");
    {
        use sqlx::ConnectOptions;
        let mut c = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true).connect().await.unwrap();
        sqlx::query(
            "CREATE TABLE bot_previews (bot_id TEXT PRIMARY KEY, host TEXT NOT NULL, pane_id TEXT, port INTEGER, dir TEXT,
             status TEXT NOT NULL, error TEXT, started_at TEXT, updated_at TEXT NOT NULL)",
        )
        .execute(&mut c)
        .await
        .unwrap();
    }
    let pool = db::open(&path).await.unwrap();
    let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('bot_previews')").fetch_all(&pool).await.unwrap();
    assert!(cols.contains(&"source".to_string()) && cols.contains(&"pid".to_string()), "{cols:?}");
    // issue #474：pre-v2 的表沒有 `REFERENCES bots(id)`，而外鍵用 ALTER 加不回去——migrate 會重建表補上。
    let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
    assert_eq!(fks, 1, "重建之後要有 bots(id) 那個外鍵");
    // 重跑一次是 no-op（已經有 FK 就不再重建），而且不能把資料洗掉。
    pool.close().await;
    let pool = db::open(&path).await.unwrap();
    let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
    assert_eq!(fks, 1, "第二次開不該再動它");
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn same_repo_by_common_dir_or_by_origin() {
    let k = |c: &str, o: Option<&str>| RepoKey { common: c.into(), origin: o.map(Into::into) };
    assert!(same_repo(&k("/a/.git", None), &k("/a/.git", None)), "同一個 repo 的 worktree");
    assert!(same_repo(&k("/a/.git", Some("git@h:x/y.git")), &k("/b/.git", Some("git@h:x/y.git"))), "各自 clone 的同一個 repo");
    assert!(!same_repo(&k("/a/.git", Some("git@h:x/y.git")), &k("/b/.git", Some("git@h:x/z.git"))));
    assert!(!same_repo(&k("/a/.git", None), &k("/b/.git", Some("git@h:x/y.git"))));
    assert!(!same_repo(&k("/a/.git", None), &k("/b/.git", None)), "沒有 origin 也對不上就不算");
}

#[test]
fn repo_key_parsing_resolves_a_relative_common_dir_and_treats_nothing_as_unknown() {
    let dir = std::env::temp_dir().join(format!("am-test-{}", db::ulid()));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let d = dir.to_string_lossy().into_owned();
    let k = parse_repo_key(&d, ".git\n", "https://h/x/y.git\n").unwrap();
    assert_eq!(k.common, std::fs::canonicalize(dir.join(".git")).unwrap().to_string_lossy());
    assert_eq!(k.origin.as_deref(), Some("https://h/x/y.git"));
    assert_eq!(parse_repo_key(&d, ".git", "").unwrap().origin, None);
    assert_eq!(parse_repo_key(&d, "  \n", "x"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relation_and_repo_name() {
    let p = |d: &str| ViteProc { pid: 1, port: 1, cwd: d.into(), kind: "vite".into() };
    let k = |c: &str, o: Option<&str>| RepoKey { common: c.into(), origin: o.map(Into::into) };
    let cands = vec![PathBuf::from("/am/web")];
    let mine = k("/am/.git", None);
    assert_eq!(relation_of(&p("/am/web/"), &cands, None, None), Relation::SameDir);
    assert_eq!(relation_of(&p("/am-main/web"), &cands, Some(&mine), Some(&k("/am/.git", None))), Relation::SameRepo);
    assert_eq!(relation_of(&p("/h/web"), &cands, Some(&mine), Some(&k("/h/.git", None))), Relation::Other);
    assert_eq!(relation_of(&p("/am-main/web"), &cands, Some(&mine), None), Relation::Other, "判不出來退成 other");
    assert_eq!(relation_of(&p("/am-main/web"), &cands, None, Some(&mine)), Relation::Other);
    assert!(Relation::SameDir < Relation::SameRepo && Relation::SameRepo < Relation::Other);
    assert_eq!(repo_name(Some(&k("/x/hermes-agents/.git", None)), "/x/hermes-agents/wt/web"), "hermes-agents");
    assert_eq!(repo_name(Some(&k("/x/bare.git", None)), "/d"), "bare.git");
    assert_eq!(repo_name(None, "/x/not-git/web/"), "web");
}

#[tokio::test]
async fn others_lists_every_local_vite_with_its_relation() {
    let r = rig().await;
    let mine = web_dir(&r);
    r.fake.repo(&r.e.repo.to_string_lossy(), "/git/am/.git", Some("git@h:me/am.git"));
    r.fake.repo(&mine, "/git/am/.git", Some("git@h:me/am.git"));
    r.fake.vite(9, 5299, &mine); // 這顆 bot 自己的目錄
    r.fake.vite(1, 5173, "/x/am-main/web"); // 同 repo 的另一個 worktree
    r.fake.repo("/x/am-main/web", "/git/am/.git", Some("git@h:me/am.git"));
    r.fake.vite(2, 5174, "/x/am-clone/web"); // 另一份 clone，origin 一樣
    r.fake.repo("/x/am-clone/web", "/git/clone/.git", Some("git@h:me/am.git"));
    r.fake.vite(3, 3001, "/x/hermes/apps/web"); // 別的 repo
    r.fake.repo("/x/hermes/apps/web", "/git/hermes-agents/.git", Some("git@h:me/hermes.git"));
    r.fake.vite(4, 3002, "/x/not-git/web"); // 判不出來
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    let got: Vec<(u64, &str, &str)> = off["others"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["port"].as_u64().unwrap(), o["relation"].as_str().unwrap(), o["repo"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![
            (5299, "same_dir", "am"),
            (5173, "same_repo", "am"),
            (5174, "same_repo", "clone"),
            (3001, "other", "hermes-agents"),
            (3002, "other", "web"),
        ]
    );
    // 5174 的 common dir 不同但 origin 相同，仍是 same_repo（repo 名取它自己的 clone 目錄）。
}

#[tokio::test]
async fn every_vite_is_other_when_the_bots_own_repo_cannot_be_read() {
    let r = rig().await;
    r.fake.vite(1, 5173, "/x/am-main/web");
    r.fake.repo("/x/am-main/web", "/git/am/.git", None); // bot 自己的目錄沒登記＝讀不到
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(off["others"][0]["relation"], "other");
}

#[tokio::test]
async fn attach_accepts_a_vite_of_another_project_and_records_its_dir() {
    let r = rig().await;
    r.fake.vite(7, 3001, "/x/hermes/apps/web");
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, req("attach", Some(3001), None)).await.unwrap();
    assert_eq!((body["source"].as_str(), body["dir"].as_str(), body["port"].as_u64()), (Some("attached"), Some("/x/hermes/apps/web"), Some(3001)));
    assert_eq!(body["pid"], 7);
    // auto 不會自己去接別的專案的（先斷開，再用 auto 起）。
    stop(&r.e.app, &bot).await.unwrap();
    let auto = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(auto["source"], "spawned");
}

#[tokio::test]
async fn a_bot_with_no_vite_config_still_gets_relations_from_its_own_dir() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let base = r.e.repo.to_string_lossy().into_owned();
    r.fake.repo(&base, "/git/wt/.git", None);
    r.fake.vite(3, 3001, "/x/wt/webui/apps/web");
    r.fake.repo("/x/wt/webui/apps/web", "/git/wt/.git", None);
    r.fake.vite(4, 3002, "/x/other/web");
    r.fake.repo("/x/other/web", "/git/other/.git", None);
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(off["candidates"], json!([]));
    let rel: Vec<(&str, &str)> =
        off["others"].as_array().unwrap().iter().map(|o| (o["relation"].as_str().unwrap(), o["repo"].as_str().unwrap())).collect();
    assert_eq!(rel, vec![("same_repo", "wt"), ("other", "other")]);
}

#[tokio::test]
async fn the_real_filesystem_search_finds_a_nested_app_and_skips_node_modules() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let app = r.e.repo.join("webui/apps/web");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("vite.config.ts"), "").unwrap();
    let nm = r.e.repo.join("node_modules/pkg");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("vite.config.js"), "").unwrap();
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(body["dir"], app.to_string_lossy().as_ref());
    assert_eq!(body["candidates"], json!([app.to_string_lossy()]));
}

#[test]
fn detect_counts_a_dev_script_and_still_looks_inside_a_monorepo_root() {
    let files: HashSet<PathBuf> = ["/p/apps/web/vite.config.ts"].iter().map(PathBuf::from).collect();
    let devs: HashSet<PathBuf> = ["/p", "/p/apps/site"].iter().map(PathBuf::from).collect();
    let subs: HashMap<PathBuf, Vec<PathBuf>> = [("/p", vec!["/p/apps"]), ("/p/apps", vec!["/p/apps/web", "/p/apps/site"])]
        .into_iter()
        .map(|(d, v)| (PathBuf::from(d), v.into_iter().map(PathBuf::from).collect()))
        .collect();
    let got = detect_dirs(Path::new("/p"), |f| files.contains(f), |d| subs.get(d).cloned().unwrap_or_default(), |d| devs.contains(d)).unwrap();
    let want: Vec<PathBuf> = ["/p", "/p/apps/site", "/p/apps/web"].iter().map(PathBuf::from).collect();
    assert_eq!(got, want, "根目錄的 dev script（turbo）不擋住裡面真正的 app");
}

fn write_pkg(dir: &Path, dev: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    let scripts = dev.map(|d| format!(r#""dev": "{d}""#)).unwrap_or_default();
    std::fs::write(dir.join("package.json"), format!(r#"{{"scripts": {{{scripts}}}}}"#)).unwrap();
}

#[tokio::test]
async fn a_dev_script_directory_is_spawned_with_bun_run_dev_and_the_port_is_observed() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("next dev"));
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(status(&body), "starting");
    // #434：allow_lan 關著，next 認得出來就接上 `-H 127.0.0.1`。
    assert_eq!(body["command"], "bun run dev -- -H 127.0.0.1");
    assert_eq!(body["kind"], "next");
    assert_eq!(body["port"], Value::Null, "不硬塞 port");
    assert_eq!(r.fake.spawns()[0].2, "bun run dev -- -H 127.0.0.1");
    assert_eq!(r.fake.spawns()[0].1, r.e.repo.to_string_lossy());
    // 還沒 listen：留在 starting。
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "starting");
    // server 自己挑了 3200：從 pane 的行程樹觀察到，記進去。
    let pane = body["pane_id"].as_str().unwrap().to_string();
    r.fake.pane_listens(&pane, 3200);
    let seen = get(&r.e.app, &bot).await.unwrap();
    assert_eq!((status(&seen), seen["port"].as_u64()), ("running", Some(3200)));
    assert_eq!(state_map(&r.e.app.db).await.unwrap()[&bot], json!({"status": "running", "port": 3200, "source": "spawned"}));
    // 之後照一般的 port 檢查：server 掛了就 failed。
    r.fake.unlisten(3200);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "failed");
}

// ── #434：allow_lan 關著時，我們起的 dev server 不准綁到 loopback 以外 ──

/// dev script 自己綁了 `0.0.0.0`（`lsof` 寫成 `*`）：旗標管不到（專案在設定檔裡蓋回去）時，
/// 起來那一拍要判成 failed，而且**把 pane 關掉**——留著等於那顆 server 繼續對外聽。
#[tokio::test]
async fn a_dev_server_that_binds_a_public_address_fails_and_its_pane_is_closed() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let pane = body["pane_id"].as_str().unwrap().to_string();

    r.fake.pane_listens_on(&pane, "*", 3200);
    let seen = get(&r.e.app, &bot).await.unwrap();

    assert_eq!(status(&seen), "failed", "綁到對外介面不能算 running：{seen}");
    let err = seen["error"].as_str().unwrap_or_default();
    assert!(err.contains('*') && err.contains("allow_lan"), "錯誤要說綁在哪與為什麼：{err}");
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane], "那顆 server 還在對外聽，pane 一定要收掉");
    assert_eq!(seen["pane_id"], Value::Null);
}

/// issue #452：起來時綁 loopback、**之後**才改綁對外的 server（設定檔改了自己重啟、dev script 內部重啟），
/// 原本永遠不會再被看一眼——bind 檢查只在 `starting → running` 那一拍跑。`running` 期間也要驗。
#[tokio::test]
async fn a_dev_server_that_rebinds_to_a_public_address_later_is_caught_too() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();

    // 第一拍乖乖綁 loopback：running，pane 留著。
    r.fake.pane_listens_on(&pane, "127.0.0.1", 3200);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert!(r.fake.closed.lock().unwrap().is_empty());

    // 之後才多綁一個對外位址（同一個 port）：下一次重驗就要抓到。
    r.fake.pane_listens_on(&pane, "*", 3200);
    let seen = get(&r.e.app, &bot).await.unwrap();

    assert_eq!(status(&seen), "failed", "起來之後才改綁對外也要擋：{seen}");
    assert!(seen["error"].as_str().unwrap_or_default().contains('*'), "{seen}");
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane], "那顆 server 正在對外聽，pane 要收掉");
}

/// `running` 期間一直綁 loopback 的，重驗幾次都不能被判失敗（這道檢查不能自己製造 flaky）。
#[tokio::test]
async fn repeated_rechecks_never_fail_a_loopback_preview() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    r.fake.pane_listens_on(&pane, "127.0.0.1", 3200);

    for i in 0..5 {
        assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running", "第 {i} 次重驗");
    }
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

/// 接上別人的 server：`running` 期間也一樣不管它綁哪裡、更不准去關它（#434 的界線，重驗不能把它拉進來）。
#[tokio::test]
async fn rechecks_never_touch_an_attached_server() {
    let r = rig().await;
    let base = r.e.repo.to_string_lossy().into_owned();
    r.fake.vite(44112, 3300, &base);
    let bot = running_bot(&r, "alfa").await;
    assert_eq!(start(&r.e.app, &bot, req("attach", Some(3300), None)).await.unwrap()["source"], "attached");

    for _ in 0..3 {
        assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    }
    assert!(r.fake.closed.lock().unwrap().is_empty(), "別人的 server 一根毛都不能動");
}

/// 綁 loopback 的照常 running——這道檢查不能把正常的預覽擋掉。
#[tokio::test]
async fn a_loopback_dev_server_is_not_affected_by_the_bind_check() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();

    r.fake.pane_listens_on(&pane, "127.0.0.1", 3200);
    r.fake.pane_listens_on(&pane, "[::1]", 3200);

    let seen = get(&r.e.app, &bot).await.unwrap();
    assert_eq!((status(&seen), seen["port"].as_u64()), ("running", Some(3200)));
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

/// `allow_lan` 開著＝使用者明講要對外（手機／Tailscale 連得到）：綁 `*` 照常 running。
#[tokio::test]
async fn the_lan_flag_allows_a_public_dev_server() {
    let mut r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    write_pkg(&r.e.repo, Some("vite"));
    Arc::get_mut(&mut r.e.app).expect("no other handle").allow_lan = true;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot, StartReq::default()).await.unwrap()["pane_id"].as_str().unwrap().to_string();

    r.fake.pane_listens_on(&pane, "*", 3200);

    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert!(r.fake.closed.lock().unwrap().is_empty());
}

/// 我們自己組的 `bunx vite --host 127.0.0.1` 也要驗：專案的 `vite.config.ts` 可以把 `server.host` 蓋回去
/// （這個 repo 的 `web/` 就是 `server.host: true`）。旗標不是保證，實際位址才是。
#[tokio::test]
async fn even_the_daemons_own_vite_command_is_verified_against_the_real_address() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let pane = body["pane_id"].as_str().unwrap().to_string();
    let port = body["port"].as_u64().unwrap() as u16;
    assert!(body["command"].as_str().unwrap().contains("--host 127.0.0.1"), "前提：旗標有帶");

    // 旗標帶了，實際卻綁在對外介面。
    r.fake.pane_listens_on(&pane, "0.0.0.0", port);

    let seen = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&seen), "failed", "{seen}");
    assert!(seen["error"].as_str().unwrap_or_default().contains("0.0.0.0"));
}

/// 問不到位址（`lsof` 讀不到）不是「綁對外」：不下結論，照原本的 port 檢查走。
#[tokio::test]
async fn an_unreadable_address_probe_never_fails_the_preview() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    r.fake.listen(body["port"].as_u64().unwrap() as u16);
    r.fake.pane_listeners_fails.store(true, std::sync::atomic::Ordering::SeqCst);

    let seen = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&seen), "running", "讀不到位址要當成不知道，不能擋下正常的預覽：{seen}");
    assert_eq!(seen["port"], body["port"]);
}

/// 接上的是別人開的 server：綁哪裡不是我們的事，也不准去關它。
#[tokio::test]
async fn an_attached_server_is_never_failed_for_its_bind_address() {
    let r = rig().await;
    let base = r.e.repo.to_string_lossy().into_owned();
    r.fake.vite(44112, 3300, &base);
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, req("attach", Some(3300), None)).await.unwrap();
    assert_eq!(body["source"], "attached");

    let seen = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&seen), "running");
    assert!(r.fake.closed.lock().unwrap().is_empty(), "別人的 server 一根毛都不能動");
}

#[tokio::test]
async fn a_dev_script_that_never_listens_fails_after_60s_with_the_pane_tail() {
    let r = rig().await;
    write_pkg(&r.e.repo.join("web"), Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    *r.fake.tail.lock().unwrap() = "error: script dev exited\n".into();
    let old = (chrono::Utc::now() - chrono::Duration::seconds(61)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE bot_previews SET started_at = ? WHERE bot_id = ?").bind(old).bind(&bot).execute(&r.e.app.db).await.unwrap();
    let body = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&body), "failed");
    assert!(body["error"].as_str().unwrap().contains("script dev exited"));
}

#[tokio::test]
async fn an_unreadable_dev_script_ports_read_does_not_count_as_a_timeout() {
    let r = rig().await;
    write_pkg(&r.e.repo.join("web"), Some("vite"));
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    let pane = body["pane_id"].as_str().unwrap().to_string();
    let old = (chrono::Utc::now() - chrono::Duration::seconds(61)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE bot_previews SET started_at = ? WHERE bot_id = ?").bind(old).bind(&bot).execute(&r.e.app.db).await.unwrap();
    r.fake.pane_listeners_fails.store(true, std::sync::atomic::Ordering::SeqCst);

    let body = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&body), "starting", "讀不到實際 ports 不能當成 60 秒都沒 listen");
    assert_eq!(body["pane_id"], pane);
    assert_eq!(body["port"], Value::Null);
}

#[tokio::test]
async fn off_state_shows_the_command_it_would_run_per_candidate() {
    let r = rig().await;
    write_pkg(&r.e.repo.join("apps/site"), Some("next dev"));
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    let web = r.e.repo.join("web").to_string_lossy().into_owned();
    let site = r.e.repo.join("apps/site").to_string_lossy().into_owned();
    assert_eq!(off["candidates"], json!([web, site]));
    assert_eq!(off["command"], "bunx vite --host 127.0.0.1 --port 5180 --strictPort");
    assert_eq!(
        off["candidate_info"],
        json!([
            {"dir": web, "command": "bunx vite --host 127.0.0.1 --port 5180 --strictPort"},
            {"dir": site, "command": "bun run dev -- -H 127.0.0.1"},
        ])
    );
}

#[tokio::test]
async fn auto_attaches_to_a_non_vite_server_running_in_the_bots_own_dir() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let base = r.e.repo.to_string_lossy().into_owned();
    r.fake.server(44112, 3200, &base, "next");
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!((body["source"].as_str(), body["kind"].as_str(), body["port"].as_u64()), (Some("attached"), Some("next"), Some(3200)));
    assert!(r.fake.spawns().is_empty());
}

#[tokio::test]
async fn others_carry_the_kind_and_a_server_in_the_project_dir_is_same_dir_even_without_candidates() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let base = r.e.repo.to_string_lossy().into_owned();
    r.fake.server(1, 3200, &base, "next");
    r.fake.server(2, 6006, "/x/other/sb", "storybook");
    let bot = running_bot(&r, "alfa").await;
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(off["candidates"], json!([]));
    let got: Vec<(&str, &str)> =
        off["others"].as_array().unwrap().iter().map(|o| (o["kind"].as_str().unwrap(), o["relation"].as_str().unwrap())).collect();
    assert_eq!(got, vec![("next", "same_dir"), ("storybook", "other")]);
}
