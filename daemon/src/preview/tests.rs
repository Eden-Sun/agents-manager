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
}

fn set(v: &[u16]) -> HashSet<u16> {
    v.iter().copied().collect()
}

// ── 純函式 ──

#[test]
fn detect_prefers_the_root_config_over_web() {
    let all = |p: &Path| p.ends_with("vite.config.ts") || p.ends_with("vite.config.mjs");
    assert_eq!(detect_dir(Path::new("/p"), all).unwrap(), PathBuf::from("/p"));
    let web_only = |p: &Path| p.starts_with("/p/web") && p.ends_with("vite.config.mts");
    assert_eq!(detect_dir(Path::new("/p"), web_only).unwrap(), PathBuf::from("/p/web"));
}

#[test]
fn detect_reports_every_path_it_tried() {
    let tried = detect_dir(Path::new("/p"), |_| false).unwrap_err();
    assert_eq!(tried.len(), 8);
    assert_eq!(tried[0], "/p/vite.config.ts");
    assert_eq!(tried[7], "/p/web/vite.config.mjs");
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
    assert_eq!(next_status(Starting, o(Some(true), true), 3), Next::To(Running, None));
    assert_eq!(next_status(Starting, o(Some(true), false), 3), Next::Stay);
    assert_eq!(next_status(Starting, o(None, false), 3), Next::Stay);
    assert!(matches!(next_status(Starting, o(Some(true), false), 60), Next::To(Failed, Some(_))));
    assert!(matches!(next_status(Starting, o(Some(false), false), 1), Next::To(Failed, Some(_))));
    // port 已經起來的那一拍，pane 讀不到不該壓過它。
    assert_eq!(next_status(Starting, o(Some(false), true), 1), Next::To(Running, None));
    assert_eq!(next_status(Running, o(Some(true), true), 999), Next::Stay);
    assert_eq!(next_status(Running, o(Some(false), true), 1), Next::To(Off, None));
    assert!(matches!(next_status(Running, o(Some(true), false), 1), Next::To(Failed, Some(_))));
    assert_eq!(next_status(Running, o(None, true), 1), Next::Stay);
    assert_eq!(next_status(Off, o(Some(true), true), 1), Next::Stay);
    assert_eq!(next_status(Failed, o(Some(true), true), 1), Next::Stay);
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
    let body = start(&r.e.app, &bot).await.unwrap();
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
    let first = start(&r.e.app, &bot).await.unwrap();
    let again = start(&r.e.app, &bot).await.unwrap();
    assert_eq!(first["pane_id"], again["pane_id"]);
    r.fake.listen(5180);
    let running = start(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&running), "running");
    assert_eq!(r.fake.spawns().len(), 1, "只開過一顆 pane");
}

#[tokio::test]
async fn two_bots_get_different_ports() {
    let r = rig().await;
    let a = running_bot(&r, "alfa").await;
    let b = running_bot(&r, "bravo").await;
    let pa = start(&r.e.app, &a).await.unwrap();
    let pb = start(&r.e.app, &b).await.unwrap();
    assert_eq!(pa["port"], 5180);
    assert_eq!(pb["port"], 5181);
}

#[tokio::test]
async fn a_port_someone_else_is_listening_on_is_skipped() {
    let r = rig().await;
    r.fake.listen(5180);
    let bot = running_bot(&r, "alfa").await;
    assert_eq!(start(&r.e.app, &bot).await.unwrap()["port"], 5181);
}

#[tokio::test]
async fn get_advances_starting_to_running_and_emits() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot).await.unwrap();
    let seq = r.e.app.current_seq();
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "starting");
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    assert!(r.e.app.current_seq() > seq, "preview_changed 有發出去");
    let map = state_map(&r.e.app.db).await.unwrap();
    assert_eq!(map[&bot], json!({"status": "running", "port": 5180}));
}

#[tokio::test]
async fn never_started_is_off_and_absent_from_state() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    assert_eq!(get(&r.e.app, &bot).await.unwrap(), json!({"status": "off"}));
    assert!(state_map(&r.e.app.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_start_that_never_listens_fails_with_the_pane_tail() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot).await.unwrap();
    *r.fake.tail.lock().unwrap() = "error: port 5180 is in use\n".into();
    let old = (chrono::Utc::now() - chrono::Duration::seconds(61)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE bot_previews SET started_at = ? WHERE bot_id = ?").bind(old).bind(&bot).execute(&r.e.app.db).await.unwrap();
    let body = get(&r.e.app, &bot).await.unwrap();
    assert_eq!(status(&body), "failed");
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("port 5180 is in use"), "{err}");
    // failed 不再佔 port：下一次啟動拿回 5180。
    assert_eq!(start(&r.e.app, &bot).await.unwrap()["port"], 5180);
}

#[tokio::test]
async fn a_running_preview_whose_vite_died_fails_and_one_whose_pane_was_closed_goes_off() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    start(&r.e.app, &bot).await.unwrap();
    r.fake.listen(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "running");
    r.fake.unlisten(5180);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "failed");

    let again = start(&r.e.app, &bot).await.unwrap();
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
    let started = start(&r.e.app, &a).await.unwrap();
    let pane = started["pane_id"].as_str().unwrap().to_string();
    assert_eq!(stop(&r.e.app, &a).await.unwrap(), json!({"status": "off"}));
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(get(&r.e.app, &a).await.unwrap(), json!({"status": "off"}));
    assert!(state_map(&r.e.app.db).await.unwrap().is_empty());
    assert_eq!(start(&r.e.app, &b).await.unwrap()["port"], 5180, "5180 放出來了");
    // 沒開過的 bot 停也是 off，不動任何 pane。
    let c = running_bot(&r, "charlie").await;
    assert_eq!(stop(&r.e.app, &c).await.unwrap(), json!({"status": "off"}));
    assert_eq!(r.fake.closed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stopping_the_bot_takes_its_preview_with_it() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    stop_for_bot(&r.e.app, &bot).await;
    assert_eq!(r.fake.closed.lock().unwrap().clone(), vec![pane]);
    assert_eq!(status(&get(&r.e.app, &bot).await.unwrap()), "off");
}

#[tokio::test]
async fn a_preview_whose_bot_no_longer_runs_is_reaped_on_the_next_look() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    let pane = start(&r.e.app, &bot).await.unwrap()["pane_id"].as_str().unwrap().to_string();
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
    let e = start(&r.e.app, &child).await.unwrap_err();
    let LcError::Conflict(v) = e else { panic!("{e:?}") };
    assert_eq!(v["reason"], "not_top_level");
    assert!(r.fake.spawns().is_empty());
}

#[tokio::test]
async fn no_vite_config_lists_what_was_tried() {
    let r = rig().await;
    std::fs::remove_file(r.e.repo.join("web/vite.config.ts")).unwrap();
    let bot = running_bot(&r, "alfa").await;
    let LcError::Conflict(v) = start(&r.e.app, &bot).await.unwrap_err() else { panic!() };
    assert_eq!(v["reason"], "no_vite_config");
    assert_eq!(v["tried"].as_array().unwrap().len(), 8);
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
    assert_eq!(start(&r.e.app, &bot).await.unwrap()["dir"], elsewhere.to_string_lossy().as_ref());

    let idle = testing::claude_bot(&r.e.app, &r.e.project_id, "bravo").await;
    let LcError::Conflict(v) = start(&r.e.app, &idle.id).await.unwrap_err() else { panic!() };
    assert_eq!(v["reason"], "bot_not_running");
}

#[tokio::test]
async fn a_refused_split_leaves_no_row_behind() {
    let r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    r.fake.spawn_fails.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(start(&r.e.app, &bot).await.is_err());
    assert!(row(&r.e.app.db, &bot).await.unwrap().is_none());
}

#[tokio::test]
async fn the_lan_flag_widens_the_bind() {
    let mut r = rig().await;
    let bot = running_bot(&r, "alfa").await;
    Arc::get_mut(&mut r.e.app).expect("no other handle").allow_lan = true;
    start(&r.e.app, &bot).await.unwrap();
    assert!(r.fake.spawns()[0].2.contains("--host 0.0.0.0"));
}

#[tokio::test]
async fn startup_reconcile_matches_rows_against_panes_and_ports() {
    let r = rig().await;
    let a = running_bot(&r, "alfa").await;
    let b = running_bot(&r, "bravo").await;
    let c = running_bot(&r, "charlie").await;
    let pa = start(&r.e.app, &a).await.unwrap()["pane_id"].as_str().unwrap().to_string();
    start(&r.e.app, &b).await.unwrap();
    start(&r.e.app, &c).await.unwrap();
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
    start(&r.e.app, &bot).await.unwrap();
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
