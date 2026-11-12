use super::*;
use std::time::Instant;

async fn app_over(dir: &std::path::Path) -> Option<Arc<App>> {
    let pool = crate::app_ports_p1::open(&dir.join("db.sqlite3")).await.ok()?;
    let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.ok()?;
    let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
    let app = App::new(pool, client.clone(), client, cfg, dir.to_path_buf(), dir.join("agents-managerd"), 7799, "t".into(), "test".into(), false);
    app.connected.store(true, Ordering::SeqCst);
    Some(app)
}

#[tokio::test]
async fn state_snapshot_and_ws_fanout_stay_cheap_on_a_production_sized_db() {
    let Ok(dir) = std::env::var("AM_STATE_BENCH_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    let app = app_over(&dir).await.expect("open the db copy");
    let bots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE deleted_at IS NULL").fetch_one(&app.db).await.unwrap();
    // 暖機一次，再量 30 次。
    let _ = state_json(&app).await.unwrap();
    let mut build = Vec::new();
    let mut ser = Vec::new();
    let mut size = 0;
    for _ in 0..30 {
        let t = Instant::now();
        let v = state_json(&app).await.unwrap();
        build.push(t.elapsed());
        let t = Instant::now();
        let bytes = serde_json::to_vec(&v).unwrap();
        ser.push(t.elapsed());
        size = bytes.len();
    }
    build.sort();
    ser.sort();
    // 哪一段貴：逐段量（同一份 DB、暖機後各 20 次取中位數）。
    async fn med<F: std::future::Future>(label: &str, mut f: impl FnMut() -> F) {
        let mut v = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            let _ = f().await;
            v.push(t.elapsed());
        }
        v.sort();
        eprintln!("PART {label}: p50={:?}", v[10]);
    }
    let ids: Vec<String> = db::live_bots(&app.db).await.unwrap().into_iter().map(|b| b.id).collect();
    med("live_projects", || db::live_projects(&app.db)).await;
    med("live_bots", || db::live_bots(&app.db)).await;
    med("unread_counts", || crate::read_marks::unread_counts(&app.db)).await;
    med("read_marks", || crate::read_marks::marks(&app.db)).await;
    med("preview_state_map", || crate::preview::state_map(&app.db)).await;
    med("all_asleep", || crate::supervisor::idle_sleep::all_asleep(&app)).await;
    med("active_run x bots", || async { for id in &ids { let _ = db::active_run(&app.db, id).await; } }).await;
    med("queued_turn x bots", || async { for id in &ids { let _ = db::queued_turn_for_bot(&app.db, id).await; } }).await;
    med("bot_connected x bots", || async { for id in &ids { let _ = app.bot_connected(id).await; } }).await;
    med("cli_updates", || crate::cli_update::running_list(&app)).await;
    med("hosts_list", || async { hosts_list(&app, &app.cfg.get().await).await }).await;
    med("cfg identities", || async { app.cfg.get().await.identities }).await;
    eprintln!("STATE bots={bots} size={size}B build p50={:?} max={:?} | serialize p50={:?} max={:?}", build[15], build[29], ser[15], ser[29]);

    // WebSocket 扇出：每個事件進環一份、`bus.send` 一份，每條連線 recv 時再 clone、再各自序列化一次。
    let payload = |n: usize| json!({"bot_id": "01M1", "turn_id": "01M2", "text": "x".repeat(n), "items": (0..20).map(|i| json!({"i": i, "k": "v"})).collect::<Vec<_>>()});
    for (label, n) in [("small(0.5KB)", 500usize), ("big(50KB)", 50_000)] {
        for clients in [0usize, 5] {
            let mut handles = Vec::new();
            let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for _ in 0..clients {
                let mut rx = app.subscribe();
                let done = done.clone();
                handles.push(tokio::spawn(async move {
                    while let Ok(ev) = rx.recv().await {
                        let _ = serde_json::to_string(&ev).unwrap();
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            }
            let events = 2000;
            let t = Instant::now();
            for _ in 0..events {
                app.emit("message_added", payload(n)).await;
                tokio::task::yield_now().await;
            }
            while done.load(Ordering::SeqCst) < events * clients {
                tokio::task::yield_now().await;
            }
            let total = t.elapsed();
            eprintln!("WS {label} clients={clients}: {events} events in {total:?} = {:?}/event", total / events as u32);
            for h in handles {
                h.abort();
            }
        }
    }
    // 20 ms 是目標；這個機器常常負載 40+，量測用的上限放寬到 50 ms，只擋「又變成逐顆查」這種數量級的退步。
    assert!(build[15] < std::time::Duration::from_millis(50), "state_json p50 {:?}", build[15]);
    assert!(size < 1_000_000, "state size {size}");
}
