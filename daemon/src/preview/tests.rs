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
        self.vites.lock().unwrap().push(ViteProc { pid, port, cwd: cwd.into() });
        self.listen(port);
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
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.alive.lock().unwrap().remove(pane_id);
            self.closed.lock().unwrap().push(pane_id.into());
        })
    }
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.listening.lock().unwrap().contains(&port) })
    }
    fn scan_vites(&self) -> BoxFuture<'_, Option<Vec<ViteProc>>> {
        Box::pin(async move { Some(self.vites.lock().unwrap().clone()) })
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
    detect_dirs(Path::new(cwd), |p| files.contains(p), |d| subs.get(d).cloned().unwrap_or_default())
}

#[test]
fn detect_lists_root_then_web_then_apps_then_packages() {
    let got = dirs(
        "/p",
        &["/p/vite.config.ts", "/p/web/vite.config.mts", "/p/apps/b/vite.config.js", "/p/apps/a/vite.config.mjs", "/p/packages/x/vite.config.ts"],
        &[("/p/apps", &["/p/apps/b", "/p/apps/a", "/p/apps/none"]), ("/p/packages", &["/p/packages/x"])],
    )
    .unwrap();
    let want: Vec<PathBuf> = ["/p", "/p/web", "/p/apps/a", "/p/apps/b", "/p/packages/x"].iter().map(PathBuf::from).collect();
    assert_eq!(got, want);
}

#[test]
fn detect_finds_a_monorepo_app_and_reports_what_it_tried() {
    let got = dirs("/p", &["/p/apps/web/vite.config.ts"], &[("/p/apps", &["/p/apps/web"])]).unwrap();
    assert_eq!(got, vec![PathBuf::from("/p/apps/web")]);
    let tried = dirs("/p", &[], &[]).unwrap_err();
    assert_eq!(tried.len(), 10);
    assert_eq!(tried[0], "/p/vite.config.ts");
    assert_eq!(tried[7], "/p/web/vite.config.mjs");
    assert_eq!(tried[8], "/p/apps/*/vite.config.*");
    assert_eq!(tried[9], "/p/packages/*/vite.config.*");
}

#[test]
fn vite_commands_are_told_apart_from_lookalikes() {
    assert!(is_vite_command("node /x/node_modules/.bin/vite --port 5173"));
    assert!(is_vite_command("bun x vite --host 0.0.0.0"));
    assert!(is_vite_command("node /x/node_modules/vite/bin/vite.js"));
    assert!(!is_vite_command("node /x/node_modules/.bin/vitest run"));
    assert!(!is_vite_command("vim vite.config.ts"));
    assert!(!is_vite_command("/usr/bin/vitepress dev"));
}

#[test]
fn ps_and_lsof_output_join_into_vite_procs() {
    let ps = "  101 node /a/node_modules/.bin/vite --port 5241
  102 vim notes
 103 bunx vite --strictPort
  104 node vitest
";
    assert_eq!(parse_ps_vites(ps), vec![101, 103]);
    let cwd = parse_lsof_cwd("p101
fcwd
n/a/web
p103
fcwd
n/b/web/
");
    assert_eq!(cwd[&101], "/a/web");
    let ports: HashMap<i32, Vec<u16>> = [(101, vec![5241]), (103, vec![5173, 5174]), (999, vec![1])].into();
    let procs = join_vites(&ports, &cwd);
    assert_eq!(procs.len(), 3, "沒有 cwd 的 999 不算");
    assert_eq!(procs[0], ViteProc { pid: 103, port: 5173, cwd: "/b/web/".into() });
}

#[test]
fn classify_attaches_only_to_the_bots_own_checkout() {
    let procs = vec![
        ViteProc { pid: 1, port: 5173, cwd: "/main/web".into() },
        ViteProc { pid: 2, port: 5241, cwd: "/mine/web/".into() },
        ViteProc { pid: 3, port: 3001, cwd: "/other/apps/web".into() },
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
    assert_eq!(stop(&r.e.app, &a).await.unwrap(), json!({"status": "off"}));
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(status(&get(&r.e.app, &a).await.unwrap()), "off");
    assert!(state_map(&r.e.app.db).await.unwrap().is_empty());
    assert_eq!(start(&r.e.app, &b, StartReq::default()).await.unwrap()["port"], 5180, "5180 放出來了");
    // 沒開過的 bot 停也是 off，不動任何 pane。
    let c = running_bot(&r, "charlie").await;
    assert_eq!(stop(&r.e.app, &c).await.unwrap(), json!({"status": "off"}));
    assert_eq!(r.fake.closed.lock().unwrap().len(), 1);
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
    assert_eq!(v["tried"].as_array().unwrap().len(), 10);
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

// ── #253 v2：接上既有的 vite ──

fn web_dir(r: &Rig) -> String {
    r.e.repo.join("web").to_string_lossy().into_owned()
}

fn req(mode: &str, port: Option<u16>, dir: Option<&str>) -> StartReq {
    StartReq { mode: Some(mode.into()), port, dir: dir.map(Into::into) }
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
    let bot = running_bot(&r, "alfa").await;
    let body = start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(body["source"], "spawned");
    assert_eq!(r.fake.spawns().len(), 1);
    assert_eq!(body["port"], 5180);
    // 起好之後停掉，回 off 的畫面要列出別份 checkout 讓使用者選。
    stop(&r.e.app, &bot).await.unwrap();
    let off = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(off["others"], json!([{"port": 5173, "dir": "/somewhere/agents-manager-main/web", "pid": 1}]));
}

#[tokio::test]
async fn disconnecting_an_attached_preview_never_touches_the_others_server() {
    let r = rig().await;
    r.fake.vite(4242, 5241, &web_dir(&r));
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot, StartReq::default()).await.unwrap();
    assert_eq!(stop(&r.e.app, &bot).await.unwrap(), json!({"status": "off"}));
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
    let _ = std::fs::remove_dir_all(&dir);
}
