
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
