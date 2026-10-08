
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_panicking_background_loop_restarts_and_shutdown_joins_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let task_attempts = attempts.clone();
        let shutdown = app.shutdown.clone();

        spawn_restartable(&app, "test loop", move || {
            let attempts = task_attempts.clone();
            let shutdown = shutdown.clone();
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("test-injected loop panic");
                }
                shutdown.cancelled().await;
            }
        });

        assert!(
            crate::testing::eventually!(attempts.load(Ordering::SeqCst) >= 2),
            "the loop should start again after its first task panics"
        );
        app.shutdown.cancel();
        app.background_tasks.close();
        app.background_tasks.wait().await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "shutdown must prevent another restart"
        );
    }

    /// 一輪 panic 之後迴圈由 supervisor 退避重啟，之後的輪次照常發生（#924）。
    #[tokio::test]
    async fn a_periodic_loop_restarts_after_a_panicking_tick() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_counter = ticks.clone();

        spawn_periodic(&app, "test periodic", Duration::from_millis(10), Duration::ZERO, move |_app| {
            let ticks = tick_counter.clone();
            async move {
                if ticks.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("test-injected tick panic");
                }
            }
        });

        assert!(
            crate::testing::eventually!(ticks.load(Ordering::SeqCst) >= 4),
            "第一輪 panic 之後迴圈要重啟、後面的輪次照常"
        );
        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait()).await.expect("shutdown 後要收乾淨");
    }

    /// 睡覺時看 shutdown：週期一小時，關機也在 2 秒內收完，不是等滿 30 秒寬限再 abort。
    #[tokio::test]
    async fn a_periodic_loop_leaves_at_shutdown_without_waiting_the_full_interval() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_counter = ticks.clone();

        spawn_periodic(&app, "test periodic long", Duration::from_secs(3600), Duration::ZERO, move |_app| {
            let ticks = tick_counter.clone();
            async move {
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        });
        assert!(crate::testing::eventually!(ticks.load(Ordering::SeqCst) == 1), "第一輪要馬上跑（first_delay = 0）");

        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait()).await.expect("睡覺中的迴圈要在 shutdown 時離開");
        assert_eq!(ticks.load(Ordering::SeqCst), 1);
    }

    /// 開出去的迴圈都掛在 `background_tasks` 下（+1），而且關機時 2 秒內收完（睡覺時有看 shutdown）。
    async fn assert_tracked_and_stops_at_shutdown(spawn: impl FnOnce(Arc<crate::state::App>)) {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let before = app.background_tasks.len();
        spawn(app.clone());
        assert_eq!(app.background_tasks.len(), before + 1, "沒有掛進 background_tasks");
        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait())
            .await
            .expect("關機時迴圈要在 2 秒內離開，不是等睡完");
    }

    #[tokio::test]
    async fn the_attachment_sweep_is_tracked_and_stops_at_shutdown() {
        assert_tracked_and_stops_at_shutdown(|app| crate::attach::spawn_sweep(app)).await;
    }

    #[tokio::test]
    async fn the_bots_trash_gc_is_tracked_and_stops_at_shutdown() {
        assert_tracked_and_stops_at_shutdown(|app| crate::bot_trash::spawn_gc(app)).await;
    }

    #[tokio::test]
    async fn the_host_baseline_poller_is_tracked_and_stops_at_shutdown() {
        assert_tracked_and_stops_at_shutdown(|app| crate::host_baseline::spawn_poller(app)).await;
    }

    #[tokio::test]
    async fn the_alias_poller_is_tracked_and_stops_at_shutdown() {
        assert_tracked_and_stops_at_shutdown(|app| crate::tools::spawn_alias_poller(app)).await;
    }

    #[tokio::test]
    async fn the_memory_poller_is_tracked_and_stops_at_shutdown() {
        assert_tracked_and_stops_at_shutdown(|app| crate::memstat::spawn_poller(app)).await;
    }

    /// 一次性的偵測也掛在追蹤之下（關機的 `wait()` 等得到它）。
    #[tokio::test]
    async fn a_one_shot_tool_detection_is_tracked() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let before = app.background_tasks.len();
        crate::tools::spawn_detect(app.clone(), "no-such-host".to_string());
        assert_eq!(app.background_tasks.len(), before + 1, "一次性偵測沒有掛進 background_tasks");
        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(5), app.background_tasks.wait()).await.expect("關機的 wait 等得到它");
    }
