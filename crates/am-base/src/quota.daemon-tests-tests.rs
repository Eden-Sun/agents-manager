
    use super::*;
    use crate::runners::quota::{clear_limit_hit_for_bot, limit_hit_for_bot, try_limit_hit_for_bot};

    /// issue #464（i407 review）：`Window::exhausted_at` 是三處共用的那一份判斷，
    /// 其中「解不開的時間戳當成已重置」沿用 `supervisor::policy::past` 的先例。
    #[test]
    fn a_window_is_exhausted_only_while_its_window_is_still_open() {
        let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc);
        let now = t("2026-09-13T12:00:00Z");
        let w = |used: f64, resets: Option<&str>| Window { observed_at: None, used_pct: used, resets_at: resets.map(String::from) };

        assert!(w(100.0, Some("2026-09-13T15:00:00Z")).exhausted_at(now), "見底、窗還沒到 → 用盡");
        assert!(!w(100.0, Some("2026-09-13T10:00:00Z")).exhausted_at(now), "見底但窗兩小時前就重置了 → 不算用盡");
        assert!(!w(10.0, Some("2026-09-13T15:00:00Z")).exhausted_at(now), "沒見底就不是用盡");
        // 解不開＝已重置（不永久擋）；沒有時間＝不知道（繼續擋）。
        assert!(!w(100.0, Some("not-a-timestamp")).exhausted_at(now), "壞掉的時間戳不該把身分永久排除");
        assert!(w(100.0, None).exhausted_at(now), "沒有重置時間就無從判斷，保守繼續擋");
        assert!(w(100.0, Some("not-a-timestamp")).reset_passed(now));
        assert!(!w(100.0, None).reset_passed(now));
    }

    /// issue #489（我 #464 帶出來的回歸）：走**真的 `set()` 路徑**。
    ///
    /// app-server 探測成功一次寫下未來的 `resets_at` → 時間跨過它、探測不再成功 → 之後只有 codex 狀態列
    /// 進來（建構時 `resets_at: None`、`used_pct` 見底）。只看「重置時刻在過去」的話，那筆繼承來的舊時刻
    /// 會把新鮮的見底讀數判成「已重置」→ 身分被當成有額度。
    #[tokio::test]
    async fn a_fresh_critical_statusline_is_not_excused_by_an_inherited_past_reset() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");
        // 1. app-server：還有額度，重置時間在「一小時前」（模擬那次探測之後時間就跨過去了）。
        let mut probe = codex_q("codex-app-server", None);
        probe.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(3));
        probe.seven_day = Some(Window {
            used_pct: 20.0,
            resets_at: Some(crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(1))),
            observed_at: None,
        });
        set(&app, LOCAL_HOST, "codex", probe).await;

        // 2. 之後只有狀態列：真的見底、沒有 resets_at。這一刻才觀測到。
        let mut status = codex_q("codex-statusline", None);
        status.updated_at = crate::db::now();
        status.seven_day = Some(Window { used_pct: 97.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "codex", status).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let now = chrono::Utc::now();
        assert_eq!(got.seven_day.as_ref().map(|w| w.used_pct), Some(97.0), "前提：新讀數真的進去了");
        assert!(
            got.exhausted(Bucket::SevenDay, now),
            "剛讀到的 97% 不能因為繼承了一個過去的重置時間就被放行：{:?}",
            got.seven_day
        );
        // 順帶：已經過去的重置時間本來就不該被沿用（對顯示也沒意義）。
        assert_eq!(got.seven_day.as_ref().and_then(|w| w.resets_at.clone()), None, "過去的 resets_at 不沿用");
    }

    /// i204 review（#489）：**解不開**的 `resets_at` 走的是票上那條原路，而且會一直黏著——
    /// `already_past` 第一版說它「沒過去」所以照樣沿用，`reset_passed` 又說它「已經跨過重置」，
    /// 於是那個身分從此永遠看起來有額度。用票上那個兩輪 `set()` 的重現釘住。
    #[tokio::test]
    async fn an_unparseable_reset_time_is_not_carried_forward_and_does_not_excuse_a_critical_reading() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");

        // 第一輪：某個來源帶進一個解不開的 resets_at。
        let mut first = codex_q("codex-app-server", None);
        first.updated_at = crate::db::now();
        first.seven_day = Some(Window { used_pct: 20.0, resets_at: Some("not-a-timestamp".into()), observed_at: None });
        set(&app, LOCAL_HOST, "codex", first).await;

        // 第二輪：狀態列讀數（沒有自己的 resets_at）而且真的見底。
        let mut status = codex_q("codex-statusline", None);
        status.updated_at = crate::db::now();
        status.seven_day = Some(Window { used_pct: 97.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "codex", status).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let w = got.seven_day.as_ref().unwrap();
        assert_eq!(w.used_pct, 97.0, "前提：新讀數進去了");
        assert_eq!(w.resets_at, None, "解不開的重置時間不該被沿用下去");
        assert!(got.exhausted(Bucket::SevenDay, chrono::Utc::now()), "97% 用掉不能因為一個解不開的時間戳就被放行");
    }

    /// i267 review（#518）：`resets_at` **與** `observed_at` 都解不開時，身分不可以被永久擋住。
    ///
    /// 這是前一顆的鏡像洞：`resets_at` 被丟成 `None` 之後就交給窗長規則收尾，但那條規則的輸入
    /// （`observed_at`，沒有就退回 `updated_at`）第一版對解不開的回 `false`＝「還很新」，
    /// 於是窗長永遠收不掉、`reset_passed` 回 false、`exhausted` 永遠 true——#475 標題那個問題原封不動回來。
    /// 既有的 `boot_drops_an_unparseable_reset_time_from_the_cache` 用的是**可解析**的 `observed_at`，走不到這裡。
    #[tokio::test]
    async fn a_reading_whose_every_timestamp_is_broken_does_not_block_forever() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut q = codex_q("boot", None);
        q.updated_at = crate::db::now();
        q.five_hour = Some(Window { used_pct: 100.0, resets_at: Some("garbage".into()), observed_at: Some("also-garbage".into()) });
        sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?,?,?)")
            .bind(&key)
            .bind(serde_json::to_string(&q).unwrap())
            .bind("not-a-timestamp") // 連整筆的 updated_at 都壞掉
            .execute(&app.db)
            .await
            .unwrap();

        load_cache(&app).await.unwrap();
        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let five = got.five_hour.as_ref().expect("窗留著");
        assert_eq!(five.resets_at, None, "壞的重置時間丟掉");
        assert_eq!(five.observed_at, None, "壞的觀測時間也丟掉");
        // 三個時間戳全壞 → 這筆讀數說不出年齡，不能拿它永久擋住一個身分。
        assert!(!got.exhausted(Bucket::FiveHour, chrono::Utc::now()), "全壞的讀數不可以永久算用盡");
        assert!(got.usable_window(Bucket::FiveHour, chrono::Utc::now()).is_none(), "當成沒有讀數");
    }

    /// 年齡判斷對解不開的時間戳要當「已過期」，而不是「還很新」（#518 的純函式那一格）。
    #[test]
    fn an_unparseable_observation_time_counts_as_expired_not_fresh() {
        let now = chrono::Utc::now();
        let mk = |observed: Option<&str>, updated: &str| Quota {
            five_hour: Some(Window { used_pct: 100.0, resets_at: None, observed_at: observed.map(String::from) }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: updated.into(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        let fresh = crate::db::iso_at(now);
        // 正常：剛觀測到、見底 → 算用盡。
        assert!(mk(Some(&fresh), &fresh).exhausted(Bucket::FiveHour, now));
        // observed_at 解不開 → 退回 updated_at（還新）→ 仍算用盡。
        assert!(mk(Some("garbage"), &fresh).exhausted(Bucket::FiveHour, now));
        // 兩個都解不開 → 說不出年齡 → 不算用盡（不永久擋人）。
        assert!(!mk(Some("garbage"), "also-garbage").exhausted(Bucket::FiveHour, now));
        assert!(!mk(None, "also-garbage").exhausted(Bucket::FiveHour, now));
    }

    /// i264 review（#489）：`load_cache` 是裸的 `serde_json::from_str`，所以快取裡解不開的 `resets_at`
    /// 原樣載回來，`reset_passed` 讀成「已重置」→ 那個身分載回來就看起來有額度。載入時要驗一次。
    #[tokio::test]
    async fn boot_drops_an_unparseable_reset_time_from_the_cache() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut q = codex_q("boot", None);
        q.updated_at = crate::db::now();
        // 見底 ＋ 解不開的重置時間：載回來不能變成「有額度」。
        q.five_hour = Some(Window { used_pct: 100.0, resets_at: Some("not-a-timestamp".into()), observed_at: Some(crate::db::now()) });
        // 好的那個要留著。
        let good = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::days(3));
        q.seven_day = Some(Window { used_pct: 10.0, resets_at: Some(good.clone()), observed_at: Some(crate::db::now()) });
        sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?,?,?)")
            .bind(&key)
            .bind(serde_json::to_string(&q).unwrap())
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();

        load_cache(&app).await.unwrap();
        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let five = got.five_hour.as_ref().expect("窗本身留著，只是沒有重置時間");
        assert_eq!(five.resets_at, None, "解不開的重置時間載回來時要丟掉");
        assert!(!five.reset_passed(chrono::Utc::now()), "沒有重置時間＝不知道，不是「已重置」");
        assert!(got.exhausted(Bucket::FiveHour, chrono::Utc::now()), "見底的讀數不能因為一個壞時間戳就被放行");
        assert_eq!(got.seven_day.as_ref().and_then(|w| w.resets_at.clone()), Some(good), "解得開的不動");
    }

    /// `unix_to_rfc3339` 的字串分支要驗過格式才放行（i204 review，#489）：
    /// 原本是原樣回傳，所以格式漂移時亂碼會直接變成 `resets_at`。
    #[test]
    fn a_reset_time_string_must_parse_before_it_is_accepted() {
        let at = |v: serde_json::Value| unix_to_rfc3339(Some(&v));
        // 正常的 RFC3339 照收，原樣留著。
        assert_eq!(at(json!("2026-09-10T00:26:40Z")), Some("2026-09-10T00:26:40Z".into()));
        // 帶時區位移的也收，原樣留著（下游一律比時刻，不比字串）。
        assert_eq!(at(json!("2026-09-10T08:26:40+08:00")), Some("2026-09-10T08:26:40+08:00".into()));
        // 解不開的丟掉，不要變成 resets_at。
        assert_eq!(at(json!("not-a-timestamp")), None);
        assert_eq!(at(json!("2026-13-99T99:99:99Z")), None);
        assert_eq!(at(json!("")), None);
        // 數字分支不受影響（秒與毫秒）。
        assert_eq!(at(json!(1_789_000_000)), Some("2026-09-10T00:26:40.000Z".into()));
        assert_eq!(at(json!(1_789_000_000_000i64)), Some("2026-09-10T00:26:40.000Z".into()));
    }

    /// #489 的第二條路：讀數**自己就帶著**一個剛過去的 `resets_at`（app-server 在窗剛翻過去時
    /// 回的就是這種），沿用那一段完全沒參與。這條專門釘 `reset_passed` 的觀測時間條件——
    /// 只靠「不沿用過期的 `resets_at`」擋不到它。
    ///
    /// （第一版我只寫了走沿用那條，結果變異「`reset_passed` 不看觀測時間」殺不掉它：
    /// 沿用那一半先把過期時刻丟了，根本走不到這個判斷。兩個守衛各自夠用，就得各自有測試。）
    #[tokio::test]
    async fn a_freshly_observed_critical_window_with_its_own_past_reset_still_blocks() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");
        let mut probe = codex_q("codex-app-server", None);
        probe.updated_at = crate::db::now();
        // 窗剛翻過去一分鐘，而這一刻讀到的就是 97% 用掉。
        probe.seven_day = Some(Window {
            used_pct: 97.0,
            resets_at: Some(crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1))),
            observed_at: None,
        });
        set(&app, LOCAL_HOST, "codex", probe).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let w = got.seven_day.as_ref().expect("讀數自己帶的 resets_at 不受沿用規則影響");
        assert!(w.resets_at.is_some(), "前提：這個過期時刻是讀數自己帶的，不是沿用來的");
        assert!(!w.reset_passed(chrono::Utc::now()), "觀測時間比重置新 → 不算跨過重置");
        assert!(got.exhausted(Bucket::SevenDay, chrono::Utc::now()), "剛讀到的 97% 要算用盡");
    }

    /// 反方向要照舊成立（#464 修的那個）：**觀測時間早於重置**的舊讀數仍然算「已重置」，不能永久擋人。
    #[test]
    fn a_reading_observed_before_the_reset_still_counts_as_reset() {
        let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc);
        let now = t("2026-09-13T12:00:00Z");
        let w = |observed: Option<&str>| Window {
            used_pct: 100.0,
            resets_at: Some("2026-09-13T10:00:00Z".into()),
            observed_at: observed.map(String::from),
        };
        // 觀測在重置之前 → 這筆讀數跨過了重置 → 不算用盡（#464）。
        assert!(w(Some("2026-09-13T09:00:00Z")).reset_passed(now));
        assert!(!w(Some("2026-09-13T09:00:00Z")).exhausted_at(now));
        // 剛好等於重置時刻也算跨過（邊界）。
        assert!(w(Some("2026-09-13T10:00:00Z")).reset_passed(now));
        // 觀測在重置之後 → 它已經反映重置後的狀態，說見底就是見底（#489）。
        assert!(!w(Some("2026-09-13T11:00:00Z")).reset_passed(now));
        assert!(w(Some("2026-09-13T11:00:00Z")).exhausted_at(now));
        // 完全不知道觀測時間 → 退回只看重置時刻（＝#464 的行為；舊快取列就是這種，本來就是舊讀數）。
        // 刻意不拿 `Quota::updated_at` 當備援，理由見 `reset_passed` 的註解。
        assert!(w(None).reset_passed(now));
        assert!(!w(None).exhausted_at(now));
        // 重置還沒到 → 無論觀測時間都不算跨過。
        let future = Window { used_pct: 100.0, resets_at: Some("2026-09-13T15:00:00Z".into()), observed_at: Some("2026-09-13T11:00:00Z".into()) };
        assert!(!future.reset_passed(now));
        assert!(future.exhausted_at(now));
    }

    /// issue #475（i267 review）：年齡要跟著**窗**走，不是跟著整筆讀數走。
    ///
    /// 走真的 `set()` 路徑：先送一筆「5h 見底、沒有 resets_at」，之後 statusline 一直只帶 7d
    /// （`set` 會把舊的 5h 原樣沿用，而 `updated_at` 蓋成現在）。只看 `updated_at` 的話那筆 5h
    /// 年齡永遠是 0，窗長到期永遠不成立——這條會紅。
    #[tokio::test]
    async fn a_carried_over_window_keeps_its_own_age_across_repeated_statuslines() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let win = |used: f64| Some(Window { used_pct: used, resets_at: None, observed_at: None });

        // 第一筆：5h 見底、7d 還有，兩個都沒有 resets_at。觀測時間是 6 小時前。
        let mut first = codex_q("statusline", None);
        first.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(6));
        first.five_hour = win(100.0);
        first.seven_day = win(10.0);
        set(&app, LOCAL_HOST, "claude", first).await;

        // 之後 statusline 只帶 7d（被截斷）：5h 被 `set` 沿用，`updated_at` 是現在。
        for _ in 0..3 {
            let mut later = codex_q("statusline", None);
            later.updated_at = crate::db::now();
            later.five_hour = None;
            later.seven_day = win(10.0);
            set(&app, LOCAL_HOST, "claude", later).await;
        }

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        assert_eq!(got.five_hour.as_ref().map(|w| w.used_pct), Some(100.0), "前提：5h 真的被沿用了");
        assert!(got.updated_at > crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1)), "前提：整筆的 updated_at 是現在");
        let observed = got.five_hour.as_ref().and_then(|w| w.observed_at.clone()).expect("沿用的窗要帶著自己的觀測時間");
        assert!(observed < crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(5)), "觀測時間要留在 6 小時前：{observed}");

        let now = chrono::Utc::now();
        assert!(!got.exhausted(Bucket::FiveHour, now), "5h 的讀數 6 小時前觀測、又沒有 resets_at → 不算用盡");
        assert!(got.usable_window(Bucket::FiveHour, now).is_none(), "當成沒有讀數");
        // 7d 每次都真的帶進來，年齡是現在，照舊算數。
        assert!(got.usable_window(Bucket::SevenDay, now).is_some());
    }

    /// 撞限記錄（繞過 `set`、只改 `limit_hit` 與 `updated_at`）不該把窗的年齡重設。
    #[tokio::test]
    async fn recording_a_limit_hit_does_not_reset_a_windows_age() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut first = codex_q("statusline", None);
        first.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(6));
        first.five_hour = Some(Window { used_pct: 100.0, resets_at: None, observed_at: None });
        first.seven_day = Some(Window { used_pct: 10.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "claude", first).await;

        let until = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(1));
        seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've reached your limit", Some("five_hour".into())).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let observed = got.five_hour.as_ref().and_then(|w| w.observed_at.clone()).expect("窗還在，觀測時間也還在");
        assert!(observed < crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(5)), "撞限記錄不該把年齡重設：{observed}");
    }

    /// issue #475（i266 review）：`resets_at` 是 `None` 的見底讀數在 `exhausted_at` 眼裡永遠用盡，
    /// 沒有任何時間能讓它翻回來。這條規則以前只在 `load_cache` 開機跑一次，所以同一次 uptime 內
    /// 照樣永久卡住——而這台 daemon 常連跑好幾天。現在每次判斷都跑。
    #[test]
    fn a_reading_older_than_its_window_stops_counting_as_exhausted() {
        let now = chrono::Utc::now();
        let q = |age: chrono::Duration, resets: Option<String>| {
            let mut x = Quota {
                five_hour: Some(Window { observed_at: None, used_pct: 100.0, resets_at: resets }),
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: crate::db::iso_at(now - age),
                source: "test".into(),
                account: None,
                host: LOCAL_HOST.into(),
            };
            x.seven_day = Some(Window { observed_at: None, used_pct: 10.0, resets_at: None });
            x
        };
        // 沒有 resets_at：窗長（5h）之內照舊算用盡，超過就不算。
        assert!(q(chrono::Duration::hours(1), None).exhausted(Bucket::FiveHour, now), "1 小時前的讀數還算數");
        assert!(!q(chrono::Duration::hours(6), None).exhausted(Bucket::FiveHour, now), "6 小時前＋沒有重置時間 → 必定跨過一次重置");
        // 回的是「沒有讀數」，不是「沒見底」：`responder` 靠這個分辨「不知道」與「有額度」。
        assert!(q(chrono::Duration::hours(6), None).usable_window(Bucket::FiveHour, now).is_none());
        assert!(q(chrono::Duration::hours(1), None).usable_window(Bucket::FiveHour, now).is_some());

        // 有 resets_at 就**不套**這條：7d 的讀數本來就可能好幾天前更新、窗卻還沒到。
        let future = Some(crate::db::iso_at(now + chrono::Duration::hours(2)));
        assert!(q(chrono::Duration::hours(6), future).exhausted(Bucket::FiveHour, now), "有重置時間就交給 reset_passed 判");
    }

    /// 窗長跟著桶走：同一筆 6 小時前的讀數，對 5h 桶算過期、對 7d／Fable 桶不算。
    #[test]
    fn the_window_length_follows_the_bucket() {
        let now = chrono::Utc::now();
        let full = Window { observed_at: None, used_pct: 100.0, resets_at: None };
        let q = Quota {
            five_hour: Some(full.clone()),
            seven_day: Some(full.clone()),
            fable: Some(full),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::iso_at(now - chrono::Duration::hours(6)),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        assert!(!q.exhausted(Bucket::FiveHour, now), "6 小時 > 5 小時窗");
        assert!(q.exhausted(Bucket::SevenDay, now), "6 小時 < 7 天窗");
        assert!(q.exhausted(Bucket::Fable, now), "Fable 跟 7d 同一個週期");
        assert_eq!(Bucket::FiveHour.len(), FIVE_HOUR_LEN);
        assert_eq!(Bucket::SevenDay.len(), SEVEN_DAY_LEN);
        assert_eq!(Bucket::Fable.len(), SEVEN_DAY_LEN);
    }

    /// **#518 刻意翻掉這條的方向。** 原本（#475）寫的是「`updated_at` 解不開就不拿年齡當理由」，
    /// 所以一筆「見底、沒有 `resets_at`、年齡又說不出來」的讀數會**永遠**算用盡——那正是 #475 標題
    /// 要修的「永久擋住一個身分」，只是換成走時間戳損毀那條路（i267 review）。
    /// 現在兩個時間戳都解不開就當成已過期＝這筆讀數不算數，跟其他四處「解不開＝已過去」同向。
    #[test]
    fn a_reading_with_no_usable_timestamp_stops_counting_as_exhausted() {
        let now = chrono::Utc::now();
        let mut q = codex_q("test", None);
        q.updated_at = "not-a-timestamp".into();
        q.five_hour = Some(Window { observed_at: None, used_pct: 100.0, resets_at: None });
        assert!(!q.exhausted(Bucket::FiveHour, now), "說不出年齡的讀數不可以永久算用盡");
        // 但 `updated_at` 讀得出來時照舊算數（這一半沒有變）。
        q.updated_at = crate::db::now();
        assert!(q.exhausted(Bucket::FiveHour, now), "年齡說得出來、又在窗長內 → 照舊算用盡");
    }

    /// issue #464 的**加固**（不是修 bug：目前沒有來源送毫秒）。1e12 秒是西元 33658 年，
    /// 所以超過門檻只可能是毫秒；不擋的話會安靜地算出一個永遠不會到的 `resets_at`。
    #[test]
    fn a_millisecond_timestamp_is_not_read_as_seconds() {
        let at = |v: serde_json::Value| unix_to_rfc3339(Some(&v));
        // 秒：照舊。
        assert_eq!(at(json!(1_789_000_000)), Some("2026-09-10T00:26:40.000Z".into()));
        // 毫秒：同一個時刻，不是西元五萬年。
        assert_eq!(at(json!(1_789_000_000_000i64)), Some("2026-09-10T00:26:40.000Z".into()));
        assert_eq!(at(json!(1_789_000_000_123i64)), Some("2026-09-10T00:26:40.123Z".into()));
        // 字串形式的毫秒一樣。
        assert_eq!(at(json!("1789000000000")), Some("2026-09-10T00:26:40.000Z".into()));
        // 已經是時間字串的原樣留著。
        assert_eq!(at(json!("2026-09-10T00:26:40Z")), Some("2026-09-10T00:26:40Z".into()));
        // 兩種都不該落在很遠的未來。
        for v in [json!(1_789_000_000_000i64), json!(1_789_000_000)] {
            let s = at(v).unwrap();
            assert!(s.starts_with("2026-"), "{s}");
        }
    }

    /// 多個 Claude session 共用一把帳號 key；較晚收到的舊窗狀態列不能蓋掉重置後的讀數。
    #[tokio::test]
    async fn an_old_statusline_snapshot_cannot_replace_newer_quota_windows() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let window = |used_pct, resets_at| Window { observed_at: None, used_pct, resets_at: Some(iso(resets_at)) };
        let mut current = codex_q("statusline", None);
        current.five_hour = Some(window(0.0, now + chrono::Duration::hours(4)));
        current.seven_day = Some(window(0.0, now + chrono::Duration::days(6)));
        current.fable = Some(window(0.0, now + chrono::Duration::days(6)));
        current.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", current).await;

        let mut stale = codex_q("statusline", None);
        stale.five_hour = Some(window(99.0, now + chrono::Duration::hours(3)));
        stale.seven_day = Some(window(99.0, now + chrono::Duration::days(5)));
        stale.fable = Some(window(99.0, now + chrono::Duration::days(5)));
        stale.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", stale).await;

        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 0.0);
    }

    #[tokio::test]
    async fn a_same_window_statusline_cannot_decrease_usage_but_usage_probe_can_correct_it() {
        let app = crate::testing::env().await.app.clone();
        let reset = chrono::Utc::now() + chrono::Duration::hours(4);
        let mut current = codex_q("statusline", None);
        current.five_hour = Some(Window { observed_at: None, used_pct: 40.0, resets_at: Some(iso(reset)) });
        current.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", current).await;

        let mut stale = codex_q("statusline", None);
        stale.five_hour = Some(Window { observed_at: None, used_pct: 10.0, resets_at: Some(iso(reset)) });
        stale.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", stale).await;
        assert_eq!(app.quotas.lock().await.get("claude").unwrap().five_hour.as_ref().unwrap().used_pct, 40.0);

        let mut probe = codex_q("claude-usage", None);
        probe.five_hour = Some(Window { observed_at: None, used_pct: 12.0, resets_at: Some(iso(reset)) });
        set(&app, LOCAL_HOST, "claude", probe).await;
        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.source, "claude-usage");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 12.0);
    }

    /// 一顆 claude session 的 statusLine：`five` 是 5h 窗 (已用, 重置)，沒有就是那顆最後一次 API 回合落在已結束的窗裡。
    fn claude_statusline(five: Option<(f64, chrono::DateTime<chrono::Utc>)>, seven: (f64, chrono::DateTime<chrono::Utc>)) -> Quota {
        let mut q = codex_q("statusline", None);
        q.five_hour = five.map(|(used_pct, at)| Window { observed_at: None, used_pct, resets_at: Some(iso(at)) });
        q.seven_day = Some(Window { observed_at: None, used_pct: seven.0, resets_at: Some(iso(seven.1)) });
        q
    }

    async fn claude_seven_day(app: &(impl QuotaTables + ?Sized)) -> f64 {
        app.quotas().lock().await.get("claude").unwrap().seven_day.as_ref().unwrap().used_pct
    }

    /// #404（2026-09-23 14:04Z 誤報 critical）：cc0 的 17 個 session 報 7d 12%，一個閒置很久的 session 報 97%，
    /// **同一個** 7d resets_at。它的 payload 沒有 5h 窗（最後一次 API 回合在已結束的 5h 窗裡），#399 的「5h 較舊就丟」
    /// 比不到；同窗取 max 就把 97 鎖住，其餘 17 個永遠壓不回去。它先到、夾在中間、最後到都一樣要是 12。
    #[tokio::test]
    async fn one_idle_session_cannot_lock_the_seven_day_window_high() {
        let now = chrono::Utc::now();
        let five_reset = now + chrono::Duration::hours(3);
        let seven_reset = now + chrono::Duration::days(2);
        let fresh = || claude_statusline(Some((6.0, five_reset)), (12.0, seven_reset));
        for idle in [
            claude_statusline(None, (97.0, seven_reset)),
            // 同一個 5h 窗、但 5h 用量比較低：也是比較舊的回合。
            claude_statusline(Some((2.0, five_reset)), (97.0, seven_reset)),
        ] {
            for position in [0, 9, 17] {
                let app = crate::testing::env().await.app.clone();
                for i in 0..18 {
                    let q = if i == position { idle.clone() } else { fresh() };
                    set(&app, LOCAL_HOST, "claude", q).await;
                }
                assert_eq!(claude_seven_day(&app).await, 12.0, "閒置 session 排在第 {position} 個：{idle:?}");
            }
        }
    }

    /// 已經被鎖在高值（例如 5h 窗全過期時收進來的舊讀數）也不是永久的：任何一筆 5h 比較新的讀數就照實寫回來。
    #[tokio::test]
    async fn a_fresher_five_hour_reading_releases_a_high_seven_day_value() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let seven_reset = now + chrono::Duration::days(2);
        let five_reset = now + chrono::Duration::hours(3);
        // 沒人用了 5 小時：沒有有效的 5h 窗可比，照舊收（同窗取大）。
        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 97.0);
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (12.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "開了新 5h 窗的讀數比較新");
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (13.0, seven_reset))).await;
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (12.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 13.0, "5h 一樣新時分不出先後，同窗照舊取大");
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((3.0, five_reset)), (12.5, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.5, "5h 用量比較高＝比較新的回合，可以往下修");
    }

    /// `/usage` 的結構化 resets_at 帶毫秒（`…:00.594Z`），statusLine 是整秒：同一個窗，不能被當成「比較舊的窗」丟掉，
    /// 也不能把 statusLine 當成比較新的窗。探測之後，閒置 session 的舊讀數一樣不能把它蓋回高值。
    #[tokio::test]
    async fn a_usage_probe_and_the_statusline_agree_on_the_window_despite_millisecond_jitter() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let five_reset = now + chrono::Duration::hours(3);
        let seven_reset = now + chrono::Duration::days(2);
        let jitter = chrono::Duration::milliseconds(594);
        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        let mut probe = codex_q("claude-usage", None);
        probe.five_hour = Some(Window { observed_at: None, used_pct: 6.0, resets_at: Some(iso(five_reset + jitter)) });
        probe.seven_day = Some(Window { observed_at: None, used_pct: 12.0, resets_at: Some(iso(seven_reset + jitter)) });
        set(&app, LOCAL_HOST, "claude", probe).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "探測直接覆寫");

        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((5.0, five_reset)), (97.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "比探測舊的 statusLine 不能把 97 蓋回來");

        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((7.0, five_reset)), (13.0, seven_reset))).await;
        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.source, "statusline", "同窗、5h 比探測多：比較新的讀數要收");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 13.0);
    }

    /// 狀態列是剩餘、存的是已用；不可洗掉 `resets_at`（2026-09-13 使用者：量表停在舊數字）。
    #[tokio::test]
    async fn the_status_line_updates_the_numbers_without_losing_the_reset_time() {
        let app = crate::testing::env().await.app.clone();
        // 重置時間用**還沒到**的相對時刻：原本寫死 2026-09-13／18，那兩個日期早就過去了，
        // 於是這條測試其實是在釘「連已經過去的重置時間也照樣沿用」——而那正是 #489 的破口
        // （繼承來的過期時刻會把新鮮的見底讀數判成已重置）。這裡的本意是「app-server 的重置時間
        // 不會被狀態列更新洗掉」，改成未來的時刻才測得到本意。
        let five_reset = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(4));
        let seven_reset = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::days(6));
        let from_server = Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(five_reset.clone()) }),
            seven_day: Some(Window { observed_at: None, used_pct: 50.0, resets_at: Some(seven_reset.clone()) }),
            fable: None,
            reset_credits: Some(ResetCredits { available: 1, title: None, expires_at: None }),
            limit_hit: None,
            plan: Some("plus".into()),
            updated_at: crate::db::now(),
            source: "codex-app-server".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "codex", from_server).await;

        let seen = crate::codex_status::parse_status_quota(
            "gpt-6-astra high · /tmp · Context 28% used · 5h 90% left · weekly 48% …",
        )
        .unwrap();
        set(&app, LOCAL_HOST, "codex", quota_from_codex_status(&seen, None).unwrap()).await;

        let q = app.quotas.lock().await.get("codex").cloned().unwrap();
        assert_eq!(q.source, "codex-statusline");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 10.0, "90% left = 10% used");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 52.0);
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at.as_deref(), Some(five_reset.as_str()), "重置時間沿用");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some(seven_reset.as_str()));
        assert!(q.reset_credits.is_some(), "重置券只有 app-server 讀得到，不能被洗掉");
    }


    #[test]
    fn a_pane_window_is_used_only_when_it_belongs_to_the_current_window() {
        let now = chrono::Utc::now();
        let h = chrono::Duration::hours;
        let len = h(5);
        let w = |used: f64, resets: chrono::DateTime<chrono::Utc>| Window { observed_at: None, used_pct: used, resets_at: Some(iso(resets)) };
        let cur = w(20.0, now + h(4)); // 窗從 1 小時前開始
        // 剛看著它變：照寫，連比較小的數字也寫（CLI 當下的說法，例如用了重置券）。
        assert_eq!(pane_window_used(Some(5.0), Some(&cur), len, Some(now), Sighting::Changed, now), Some(5.0));
        // 同一個窗：只增不減。
        assert_eq!(pane_window_used(Some(30.0), Some(&cur), len, Some(now - h(0)), Sighting::Same, now), Some(30.0));
        assert_eq!(pane_window_used(Some(10.0), Some(&cur), len, Some(now), Sighting::Same, now), None);
        // 畫面比這個窗還舊：不管 New 還是 Same 都不採用。
        assert_eq!(pane_window_used(Some(90.0), Some(&cur), len, Some(now - h(2)), Sighting::New, now), None);
        // 記著的窗已經過了重置：畫面要晚於那次重置才採用。
        let ended = w(90.0, now - h(1));
        assert_eq!(pane_window_used(Some(3.0), Some(&ended), len, Some(now - h(2)), Sighting::Same, now), None);
        assert_eq!(pane_window_used(Some(3.0), Some(&ended), len, Some(now - chrono::Duration::minutes(30)), Sighting::Same, now), Some(3.0));
        // 沒有重置時間：只收這個行程第一次看到的畫面。
        let bare = Window { observed_at: None, used_pct: 20.0, resets_at: None };
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::New, now), Some(7.0));
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::Same, now), None);
        assert_eq!(pane_window_used(None, Some(&cur), len, Some(now), Sighting::Changed, now), None);
    }

    /// 截斷只讀到 5h 時不可洗掉 7d（2026-09-13 使用者：header 的 codex 只剩一條）。
    #[tokio::test]
    async fn a_partial_reading_keeps_the_window_it_could_not_see() {
        let app = crate::testing::env().await.app.clone();
        let mut full = codex_q("codex-app-server", None);
        full.five_hour = Some(Window { observed_at: None, used_pct: 30.0, resets_at: Some("2026-09-13T19:22:00.000Z".into()) });
        full.seven_day = Some(Window { observed_at: None, used_pct: 76.0, resets_at: Some("2026-09-18T00:00:00.000Z".into()) });
        set(&app, LOCAL_HOST, "codex", full).await;

        let mut partial = codex_q("codex-statusline", None);
        partial.five_hour = Some(Window { observed_at: None, used_pct: 64.0, resets_at: None });
        partial.seven_day = None;
        set(&app, LOCAL_HOST, "codex", partial).await;

        let q = app.quotas.lock().await.get("codex").cloned().unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 64.0, "看得到的那條要更新");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 76.0, "看不到的那條沿用，不是清空");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-18T00:00:00.000Z"));
    }

    fn env(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 2026-09-14 第二次冒出兩個 codex（AGM 交辦）：cc1 只設 `CLAUDE_CONFIG_DIR`，對 codex 它仍是預設
    /// 帳號；要看的是**該 kind 的 home 變數**，不是 env 空不空。
    #[test]
    fn an_identity_shares_the_default_account_unless_it_sets_that_kinds_home() {
        let cc0 = env(&[]);
        let cc1 = env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]);
        let cc2 = env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc2"), ("CODEX_HOME", "$HOME/.codex-cc2")]);
        assert!(identity_shares_default("codex", &cc0));
        assert!(identity_shares_default("codex", &cc1), "cc1 沒有 CODEX_HOME：對 codex 就是預設帳號");
        assert!(!identity_shares_default("codex", &cc2), "cc2 有自己的 CODEX_HOME 才分開");
        assert!(identity_shares_default("claude", &cc0));
        assert!(!identity_shares_default("claude", &cc1), "對 claude，cc1 有自己的 config dir");
        assert!(identity_shares_default("grok", &cc2), "沒有 GROK_HOME 的身分對 grok 是預設帳號");
    }

    /// 寫入端與查詢端走同一支：帶 claude 身分（cc1）的 codex bot 寫裸 `codex`、`limit_hit_for_bot` 也從裸
    /// `codex` 讀到；只有 codex 自己的身分（cx2）才寫 `codex:cx2`，而且**不借**裸 `codex` 的數字。
    /// 回歸（2026-09-16）：重啟那一秒身分還沒偵測完，cc0 的讀數先落在 `claude:cc0`；偵測完之後寫裸 `claude` 時，
    /// 那一格要清掉。有自己帳號目錄的 cc1、查不到的身分都照舊保留，遠端主機的同名 key 不受本機影響。
    #[tokio::test]
    async fn a_split_key_left_from_before_identities_were_known_is_dropped_once_they_are() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let reading = |pct: f64| {
            let mut q = codex_q("statusline", None);
            q.five_hour = Some(Window { observed_at: None, used_pct: pct, resets_at: None });
            q
        };
        // 身分還沒偵測到：cc0 寧可分開。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc0")).await, "claude:cc0");
        set(&app, LOCAL_HOST, "claude:cc0", reading(1.0)).await;
        set(&app, LOCAL_HOST, "claude:cc1", reading(40.0)).await;
        set(&app, LOCAL_HOST, "claude:nobody", reading(50.0)).await;
        set(&app, "m4p", "claude:cc0", reading(60.0)).await;

        let ident = |name: &str, pairs: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
            env: env(pairs),
            args: vec![],
        };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![ident("cc0", &[]), ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")])],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        // 偵測完：cc0 收斂到裸 key。下一筆讀數寫裸 key 的同時把殘留的那一格清掉。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc0")).await, "claude");
        set(&app, LOCAL_HOST, "claude", reading(25.0)).await;
        let q = app.quotas.lock().await;
        assert!(q.get("claude:cc0").is_none(), "殘留的分開那格清掉");
        assert_eq!(q.get("claude").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(25.0));
        assert!(q.get("claude:cc1").is_some(), "有自己帳號目錄的照舊分開");
        assert!(q.get("claude:nobody").is_some(), "查不到的身分寧可保留");
        assert!(q.get(&quota_key("m4p", "claude:cc0")).is_some(), "別台主機的不受影響");
    }

    #[tokio::test]
    async fn codex_bots_on_cc1_share_the_bare_key_and_cc2_keeps_its_own() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let ident = |name: &str, pairs: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
            env: env(pairs),
            args: vec![],
        };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![
                    ident("cc0", &[]),
                    ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]),
                    ident("cc2", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc2"), ("CODEX_HOME", "$HOME/.codex-cc2")]),
                    // codex 自己的身分（kind = codex）才可能分開成 `codex:<name>`。
                    crate::config::IdentityCfg {
                        name: "cx2".into(),
                        kind: "codex".into(),
                        host: None,
                        env: env(&[("CODEX_HOME", "$HOME/.codex-cx2")]),
                        args: vec![],
                    },
                ],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc1")).await, "codex");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc0")).await, "codex");
        // 2026-09-14 使用者指正：ccN 是 Claude Code 的帳號代號，就算 cc2 設了 CODEX_HOME，它仍是 claude 的身分，
        // codex 不該有 `codex:cc2`。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc2")).await, "codex");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cx2")).await, "codex:cx2", "codex 自己的身分才分開");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc1")).await, "claude:cc1");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("nobody")).await, "codex:nobody", "查不到的身分寧可分開");

        // 裸 codex 撞限：cc1 的 codex bot 讀得到，cc2 的讀不到（它有自己的帳號）。
        let hit = LimitHit { message: "You've hit your usage limit.".into(), until: Some("2999-01-01T00:00:00Z".into()), at: crate::db::now(), bucket: None };
        let mut q = codex_q("codex-limit-hit", Some(hit));
        q.five_hour = Some(Window { observed_at: None, used_pct: 100.0, resets_at: None });
        set(&app, LOCAL_HOST, "codex", q).await;
        let bot = |identity: &str| crate::db::Bot {
            id: format!("b-{identity}"),
            project_id: "p".into(),
            name: identity.into(),
            kind: "codex".into(),
            model: None,
            effort: None,
            fast: 0,
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: Some(identity.into()),
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: crate::db::now(),
        };
        assert!(limit_hit_for_bot(&app, &bot("cc1")).await.is_some(), "cc1 的 codex bot 讀的是裸 codex");
        assert!(limit_hit_for_bot(&app, &bot("cx2")).await.is_none(), "cx2 是 codex 自己的另一個帳號，不借預設帳號的撞限");
    }

    /// AGM 的條件：寫入 key 要跟 `limit_hit_for_bot` 查法對得起來，且不能洗掉「撞上限」。
    #[tokio::test]
    async fn the_status_line_writes_where_the_lookup_reads_and_keeps_the_limit_hit() {
        let app = crate::testing::env().await.app.clone();
        let base = quota_base("codex", Some("astra"));
        assert_eq!(base, "codex:astra");
        assert_eq!(quota_base("codex", None), "codex");
        // 2026-09-14 使用者：額度列冒出第二個 codex。對 codex 共用預設帳號的身分寫裸 key。
        assert_eq!(quota_base_default_aware("codex", Some("cc0"), true), "codex");
        assert_eq!(quota_base_default_aware("codex", Some("cc2"), false), "codex:cc2");
        assert_eq!(quota_base_default_aware("codex", None, true), "codex");
        assert_eq!(quota_base_default_aware("claude", Some("cc1"), false), "claude:cc1");
        assert_eq!(quota_base("codex", Some("  ")), "codex", "空白身分就是沒指定");

        // 清掉的話 assignment 會立刻又派工過去（718d025 的 quota_blocked 靠這一格）。
        let hit = LimitHit {
            message: "You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00Z".into()),
            at: crate::db::now(),
            bucket: None,
        };
        let mut server = quota_from_codex_status(
            &crate::codex_status::CodexStatusQuota { five_hour_left: Some(50.0), weekly_left: Some(50.0) },
            Some("astra"),
        )
        .unwrap();
        server.limit_hit = Some(hit);
        server.source = "codex-app-server".into();
        set(&app, LOCAL_HOST, &base, server).await;

        let fresh = quota_from_codex_status(
            &crate::codex_status::CodexStatusQuota { five_hour_left: Some(90.0), weekly_left: Some(48.0) },
            Some("astra"),
        )
        .unwrap();
        assert!(fresh.limit_hit.is_none(), "狀態列本來就讀不到這一格");
        set(&app, LOCAL_HOST, &base, fresh).await;

        let q = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, &base)).cloned().unwrap();
        assert_eq!(q.source, "codex-statusline", "來源分得出來");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 10.0);
        assert!(q.limit_hit.is_some(), "撞上限那一格要留著");
    }

    /// SPEC §14.3：同輪各主機併發。兩台都要等對方到齊才放行——串列跑就會卡到逾時。
    #[tokio::test]
    async fn hosts_are_polled_at_the_same_time() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let run = for_each_host(vec!["local".into(), "m4p".into()], |_host| {
            let (barrier, done) = (barrier.clone(), done.clone());
            async move {
                barrier.wait().await;
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), run).await.expect("一台一台跑會卡在 barrier");
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 2, "全部跑完才回來");
    }

    #[test]
    fn a_status_line_without_numbers_is_not_a_reading() {
        let empty = crate::codex_status::CodexStatusQuota { five_hour_left: None, weekly_left: None };
        assert!(quota_from_codex_status(&empty, None).is_none());
    }

    #[tokio::test]
    async fn a_statusline_reading_keeps_the_probes_fable_window() {
        let app = crate::testing::env().await.app.clone();
        let probe = Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 20.0, resets_at: None }),
            fable: Some(Window { observed_at: None, used_pct: 66.0, resets_at: None }),
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "claude-usage".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", probe).await;
        let status = Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 11.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 21.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "statusline".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "claude:cc1")).cloned().unwrap();
        assert_eq!(got.five_hour.unwrap().used_pct, 11.0, "the fresher 5h wins");
        assert_eq!(got.fable.unwrap().used_pct, 66.0, "the Fable window the statusLine cannot see survives");
    }

    use super::*;

    /// review3 c3 H2：Fable 桶只擋跑 Fable 的 bot；5h／7d／沒有桶名的擋整個帳號；不知道 bot 在跑什麼模型時保守地擋。
    #[test]
    fn a_model_bucket_only_blocks_bots_on_that_model() {
        for (bucket, model, blocks) in [
            (Some("fable"), Some("fable"), true),
            (Some("fable"), Some("claude-fable-5-1"), true),
            (Some("fable"), Some("Fable 5.1"), true),
            (Some("fable"), Some("opus"), false),
            (Some("fable"), Some("opus[1m]"), false),
            (Some("fable"), Some("sonnet"), false),
            (Some("fable"), None, true),
            (Some("fable"), Some("default"), true),
            // Opus／Sonnet 的週桶同理（review3 c4 M1）。
            (Some("opus"), Some("opus"), true),
            (Some("opus"), Some("claude-opus-5"), true),
            (Some("opus"), Some("fable"), false),
            (Some("opus"), Some("sonnet"), false),
            (Some("sonnet"), Some("sonnet"), true),
            (Some("sonnet"), Some("opus"), false),
            (Some("opus"), None, true),
            (Some("five_hour"), Some("opus"), true),
            (Some("seven_day"), Some("opus"), true),
            (None, Some("opus"), true),
            (None, Some("gpt-5.6-sol"), true),
        ] {
            assert_eq!(bucket_blocks_model(bucket, model), blocks, "{bucket:?} × {model:?}");
        }
    }

    fn codex_q(source: &str, limit_hit: Option<LimitHit>) -> Quota {
        Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 0.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 0.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: None,
            updated_at: crate::db::now(),
            source: source.into(),
            account: None,
            host: LOCAL_HOST.into(),
        }
    }

    /// 2026-09-12 使用者：量表全滿卻一直 hit limit；橫幅要黏過 app-server 輪詢。
    #[tokio::test]
    async fn a_codex_limit_hit_outlives_the_app_server_poll() {
        let app = crate::testing::env().await.app.clone();
        // 時間寫死：2026-09-13 用 `db::now()` 時 6 跑 2 敗（跨毫秒變成另一情境）。
        let hit = LimitHit {
            message: "ERROR: You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00.000Z".into()),
            at: "2026-09-13T14:15:30.000Z".into(),
            bucket: None,
        };
        let mut blocked = codex_q("codex-limit-hit", Some(hit));
        blocked.updated_at = "2026-09-13T14:15:30.000Z".into();
        set(&app, LOCAL_HOST, "codex", blocked).await;
        let mut poll = codex_q("codex-app-server", None);
        poll.updated_at = "2026-09-13T14:21:00.000Z".into();
        let got_app = app.clone();
        let got = || {
            let app = got_app.clone();
            async move { app.quotas().lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap() }
        };
        // 2026-09-12 不變量：只有 `until` 到了或 `clear_limit_hit` 才能清掉。
        assert!(got().await.limit_hit.is_some(), "量表滿了不代表 CLI 收得下一句話");
        clear_limit_hit(&app, LOCAL_HOST, "codex").await;
        assert!(got().await.limit_hit.is_none());
    }

    fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
        // 跟生產端同一支：格式只有一種（issue #101）。
        crate::db::iso_at(t)
    }

    /// M2（review 2026-09-16）：Fable 撞限時那一桶還沒讀數，保底 7 天；之後 `/usage` 的真讀數要能把它縮短，
    /// 窗重置之後的讀數要能把它清掉。claude 沒有成功回合清撞限這條路，`until` 是唯一出口。
    #[tokio::test]
    async fn a_claude_hit_is_corrected_by_later_readings_of_its_own_bucket() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::hours(1);
        let hit = LimitHit {
            message: "You've reached your Fable limit".into(),
            until: Some(iso(at + chrono::Duration::days(7))),
            at: iso(at),
            bucket: Some("fable".into()),
        };
        let mut banner = codex_q("claude-limit-hit", Some(hit));
        banner.five_hour = None;
        banner.seven_day = None;
        set(&app, LOCAL_HOST, "claude:cc1", banner).await;
        let until_app = app.clone();
        let until = || {
            let app = until_app.clone();
            async move {
                app.quotas().lock().await.get("claude:cc1").unwrap().limit_hit.as_ref().map(|h| h.until.clone().unwrap())
            }
        };

        // 撞限後讀到的 Fable 窗：還見底，明天 08:00 重置 → 撞限最晚到那時，不是下週。
        let tomorrow = now + chrono::Duration::hours(20);
        let mut usage = codex_q("claude-usage", None);
        usage.fable = Some(Window { observed_at: None, used_pct: 100.0, resets_at: Some(iso(tomorrow)) });
        set(&app, LOCAL_HOST, "claude:cc1", usage.clone()).await;
        assert_eq!(until().await, Some(iso(tomorrow)), "保底的 7 天要被那一桶自己的重置時間截短");

        // 不相干的桶（statusLine 只有 5h／7d）不算那一桶的讀數。
        let mut status = codex_q("statusline", None);
        status.five_hour = Some(Window { observed_at: None, used_pct: 3.0, resets_at: Some(iso(now + chrono::Duration::hours(4))) });
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        assert_eq!(until().await, Some(iso(tomorrow)));

        // 重置之後的讀數：窗的起點在撞限之後 → 撞限作廢。
        let mut after = codex_q("claude-usage", None);
        after.fable = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(at + chrono::Duration::days(7) + chrono::Duration::minutes(1))) });
        set(&app, LOCAL_HOST, "claude:cc1", after).await;
        assert_eq!(until().await, None, "那一桶重置過了，撞限不能再擋");
    }

    /// 撞限前就開始、撞限後才回來的讀數（百分比可能還沒到頂）不能把撞限清掉，只能截短時間。
    /// 沒有桶名的撞限（codex credits 用完、開機回填）完全不動。
    #[test]
    fn a_reading_from_before_the_hit_only_shortens_it_and_a_bucketless_hit_is_left_alone() {
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::minutes(10);
        let hit = |bucket: Option<&str>| LimitHit {
            message: "You've hit your session limit".into(),
            until: Some(iso(at + chrono::Duration::hours(5))),
            at: iso(at),
            bucket: bucket.map(String::from),
        };
        let mut reading = codex_q("statusline", None);
        reading.five_hour = Some(Window { observed_at: None, used_pct: 94.0, resets_at: Some(iso(now + chrono::Duration::minutes(20))) });
        let got = recalibrate_limit_hit(hit(Some("five_hour")), &reading).expect("窗在撞限之前就開了：還在擋");
        assert_eq!(got.until, Some(iso(now + chrono::Duration::minutes(20))));
        assert_eq!(recalibrate_limit_hit(hit(None), &reading), Some(hit(None)), "沒有桶名就不猜");
        let mut later = reading.clone();
        later.five_hour.as_mut().unwrap().resets_at = Some(iso(at + chrono::Duration::hours(6)));
        assert_eq!(recalibrate_limit_hit(hit(Some("five_hour")), &later), None);
    }

    /// 2026-09-27：codex 撞限（沒有桶名）之後提早重置，新讀數 5h 與 7d 兩個窗都是撞限之後才開的——整個帳號重置過了，撞限作廢。
    /// 只有一桶重開（5h 自然滾動）不算：credits 用完的撞限量表看不到，不能靠 5h 重開就放行。
    #[test]
    fn a_bucketless_hit_is_void_once_both_windows_reopened_after_it() {
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::days(1);
        let hit = LimitHit {
            message: "You've hit your usage limit ... try again at Sep 28th".into(),
            until: Some(iso(now + chrono::Duration::days(1))),
            at: iso(at),
            bucket: None,
        };
        let mut both = codex_q("codex-app-server", None);
        both.five_hour = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(now + chrono::Duration::hours(5))) });
        both.seven_day = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(now + chrono::Duration::days(7))) });
        assert_eq!(recalibrate_limit_hit(hit.clone(), &both), None, "兩桶都在撞限後重開：作廢");

        let mut only_5h = both.clone();
        only_5h.seven_day = Some(Window { observed_at: None, used_pct: 100.0, resets_at: Some(iso(now + chrono::Duration::days(1))) });
        assert_eq!(recalibrate_limit_hit(hit.clone(), &only_5h), Some(hit.clone()), "只有 5h 重開：照擋");

        let mut no_7d = both.clone();
        no_7d.seven_day = None;
        assert_eq!(recalibrate_limit_hit(hit.clone(), &no_7d), Some(hit), "讀不到 7d 不猜");
    }

    /// #236：窗在撞限**之前**就結束的讀數（閒置的 5h 窗：`/usage` 照樣回上一個重置時間）說不出這次撞限的事——
    /// 拿它取 `min` 會把 `until` 拉到過去、撞限當場作廢，派工照送。撞限原樣留著，經過 `set` 也一樣。
    #[tokio::test]
    async fn a_reading_of_a_window_that_ended_before_the_hit_leaves_it_alone() {
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::minutes(10);
        let hit = LimitHit {
            message: "You've hit your session limit".into(),
            until: Some(iso(at + chrono::Duration::hours(5))),
            at: iso(at),
            bucket: Some("five_hour".into()),
        };
        let mut idle = codex_q("claude-usage", None);
        idle.five_hour = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(now - chrono::Duration::hours(1))) });
        assert_eq!(recalibrate_limit_hit(hit.clone(), &idle), Some(hit.clone()), "上一個窗的讀數不動它");

        let app = crate::testing::env().await.app.clone();
        let mut banner = codex_q("claude-limit-hit", Some(hit.clone()));
        banner.five_hour = None;
        banner.seven_day = None;
        set(&app, LOCAL_HOST, "claude:cc1", banner).await;
        set(&app, LOCAL_HOST, "claude:cc1", idle).await;
        let got = app.quotas.lock().await.get("claude:cc1").unwrap().limit_hit.clone();
        assert_eq!(got, Some(hit), "經過 set 也還擋著");
    }

    /// #238：run 在跑的時候改身分（PATCH 回 `needs_restart`）：按「重啟」之前 pane 裡還是**起來時的帳號**。額度讀數、撞限、
    /// 閘門、成功回合清撞限都要記在那個身分上；以前一律看 `bots.identity`（已經是新的）——舊帳號用盡記到新帳號名下，
    /// 新帳號被誤擋、舊帳號的其他 bot 照樣被派工。重啟之後才換成新身分。
    #[tokio::test]
    async fn a_run_bills_the_identity_it_started_with_until_it_is_restarted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        for name in ["cc1", "cc2"] {
            let dir = env.dir.join(format!("claude-{name}")).to_string_lossy().into_owned();
            app.cfg
                .update(move |c| {
                    c.identities.push(crate::config::IdentityCfg {
                        name: name.into(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), dir)].into(),
                        args: vec![],
                    });
                    Ok(())
                })
                .await
                .unwrap();
        }
        let bot = crate::testing::claude_bot(&app, &env.project_id, "switcher").await;
        sqlx::query("UPDATE bots SET identity='cc1' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        crate::lifecycle::start_bot(&app, &bot.id).await.unwrap();
        let run = crate::db::active_run(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.started_identity(), Some(Some("cc1".to_string())), "起來時的身分蓋在 run 上");

        // 使用者把身分改成 cc2、還沒重啟：pane 還是 cc1 的帳號。
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let status = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StatusLine", "rate_limits": {"five_hour": {"used_percentage": 42.0, "resets_at": (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp()}}}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &status).await.unwrap();
        {
            let q = app.quotas.lock().await;
            let keys: Vec<String> = q.keys().cloned().collect();
            assert_eq!(q.get("claude:cc1").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(42.0), "讀數記在 cc1：{keys:?}");
            assert!(q.get("claude:cc2").is_none(), "新身分那一格沒被寫：{keys:?}");
        }

        crate::turn_error::mark_claude_limit_hit(&app, &bot, "You've hit your session limit · resets 5pm").await.unwrap();
        let hits: Vec<String> = app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect();
        assert_eq!(hits, vec!["claude:cc1".to_string()], "撞限記在實際撞到的帳號");
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "閘門照 cc1 擋：排著的會送進 cc1 的行程");
        clear_limit_hit_for_bot(&app, &bot).await;
        assert!(app.quotas.lock().await["claude:cc1"].limit_hit.is_none(), "成功回合清的是 cc1");

        // 重啟之後才是 cc2：cc1 的撞限不再擋它，cc2 的才擋。
        crate::turn_error::mark_claude_limit_hit(&app, &bot, "You've hit your session limit · resets 5pm").await.unwrap();
        crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap();
        crate::lifecycle::start_bot(&app, &bot.id).await.unwrap();
        let run = crate::db::active_run(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.started_identity(), Some(Some("cc2".to_string())));
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "重啟成 cc2：cc1 的撞限不擋它");
        assert_eq!(billing_identity(&app, &bot).await.unwrap().as_deref(), Some("cc2"));
    }

    /// run 沒記身分（不是 daemon 起的、升級前的舊列）照 bot 設定的；記了空字串＝起來時沒有身分（預設帳號），就算之後設了身分也一樣。
    #[tokio::test]
    async fn a_run_without_a_recorded_identity_falls_back_to_the_bots() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "fallback").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let case = |bot_identity: Option<&'static str>, recorded: Option<&'static str>| {
            let (app, bot_id, run_id) = (app.clone(), bot.id.clone(), run_id.clone());
            async move {
                sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(bot_identity).bind(&bot_id).execute(&app.db).await.unwrap();
                sqlx::query("UPDATE runs SET runtime_identity=? WHERE id=?").bind(recorded).bind(&run_id).execute(&app.db).await.unwrap();
                let bot = crate::db::bot(&app.db, &bot_id).await.unwrap().unwrap();
                billing_identity(&app, &bot).await.unwrap()
            }
        };
        assert_eq!(case(Some("cc2"), None).await.as_deref(), Some("cc2"), "run 沒記：照 bot 設定的");
        assert_eq!(case(Some("cc2"), Some("cc1")).await.as_deref(), Some("cc1"), "run 記了就用它");
        assert_eq!(case(Some("cc2"), Some("")).await, None, "起來時沒有身分（預設帳號）");
        assert_eq!(case(None, Some(" cc1 ")).await.as_deref(), Some("cc1"));
        assert_eq!(case(Some("  "), None).await, None);
        crate::lifecycle::stop_bot(&app, &bot.id).await.ok();
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(billing_identity(&app, &bot).await.unwrap().as_deref(), Some("cc2"), "沒有 run：照 bot 設定的");
    }

    #[tokio::test]
    async fn a_limit_hit_past_its_reset_time_is_dropped() {
        let app = crate::testing::env().await.app.clone();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let hit = LimitHit { message: "ERROR: You've hit your usage limit.".into(), until: Some(past), at: crate::db::now(), bucket: None };
        set(&app, LOCAL_HOST, "codex", codex_q("codex-limit-hit", Some(hit))).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap();
        assert!(got.limit_hit.is_none(), "過了恢復時間的橫幅不該再擋著畫面");
    }

    /// codex 當天只寫 `try again at 5:07 AM`，解析不出來寧可留著等下一回合成功再清。
    #[test]
    fn a_limit_hit_without_a_time_never_expires_on_its_own() {
        let hit = LimitHit { message: "ERROR: usage limit".into(), until: None, at: crate::db::now(), bucket: None };
        assert!(!limit_hit_expired(Some(&hit)));
        assert!(!limit_hit_expired(None));
    }

    #[test]
    fn codex_rate_limits_map_by_window() {
        let r = json!({"rateLimits": {
            "primary": {"usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1788650185},
            "secondary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1789179340},
            "planType": "plus"
        }});
        let q = quota_from_codex(&r).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 18.0);
        assert!(q.seven_day.unwrap().resets_at.unwrap().starts_with("2026-"));
        assert_eq!(q.plan.as_deref(), Some("plus"));
        assert_eq!(q.source, "codex-app-server");
    }

    /// 2026-09-10 使用者：額度用完時的重置券。
    #[test]
    fn codex_reset_credits_are_read_with_the_windows() {
        let r = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1789074446},
                "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1789450308},
                "planType": "plus"
            },
            "rateLimitResetCredits": {
                "availableCount": 1,
                "credits": [
                    {"status": "used", "title": "已經用掉的那張", "expiresAt": 1791173488},
                    {"status": "available", "title": "Full reset (Weekly + 5 hr)", "expiresAt": 1791173488}
                ]
            }
        });
        let c = quota_from_codex(&r).unwrap().reset_credits.unwrap();
        assert_eq!(c.available, 1);
        assert_eq!(c.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert!(c.expires_at.unwrap().starts_with("2026-"));
    }

    #[test]
    fn no_reset_credits_field_means_none() {
        let r = json!({"rateLimits": {"primary": {"usedPercent": 3, "windowDurationMins": 300}}});
        assert!(quota_from_codex(&r).unwrap().reset_credits.is_none());
    }

    #[test]
    fn keys_are_host_scoped() {
        assert_eq!(quota_key("local", "claude"), "claude");
        assert_eq!(quota_key("local", "claude:cc1"), "claude:cc1");
        assert_eq!(quota_key("m4p", "claude:cc1"), "m4p/claude:cc1");
        let hosts = vec!["local".to_string(), "m4p".to_string()];
        assert_eq!(host_of_key("claude", &hosts), ("local", "claude"));
        assert_eq!(host_of_key("m4p/claude:cc1", &hosts), ("m4p", "claude:cc1"));
        assert_eq!(host_of_key("gone/claude", &hosts), ("local", "gone/claude"));
    }


    /// issue #392：讀數寫入快取，重啟後先以 stale 回填；新的探測成功後才恢復 fresh，過期的舊撞限也不能卡住派送。
    #[tokio::test]
    async fn quota_cache_survives_restart_as_stale_until_a_fresh_reading_arrives() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let reading = |used_pct: f64, limit_hit: Option<LimitHit>| Quota {
            five_hour: Some(Window { observed_at: None, used_pct, resets_at: Some("2099-01-01T00:00:00Z".into()) }),
            seven_day: Some(Window { observed_at: None, used_pct: 20.0, resets_at: Some("2099-01-07T00:00:00Z".into()) }),
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: Some("test".into()),
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };

        set(&app, LOCAL_HOST, "claude", reading(41.0, None)).await;
        let restarted = crate::testing::restart_app(&env).await;
        assert_eq!(load_cache(&restarted).await.unwrap(), 1);
        assert_eq!(restarted.quotas.lock().await["claude"].five_hour.as_ref().unwrap().used_pct, 41.0);
        assert_eq!(snapshot(&restarted).await["kinds"]["claude"]["stale"], true);

        let fresh = reading(52.0, None);
        set(&restarted, LOCAL_HOST, "claude", fresh).await;
        assert_eq!(snapshot(&restarted).await["kinds"]["claude"]["stale"], false);
        assert_eq!(restarted.quotas.lock().await["claude"].five_hour.as_ref().unwrap().used_pct, 52.0);

        // 模擬快取裡還留著一筆已過 reset 的舊「用完了」；開機回填時照既有規則清掉，不能阻擋新的工作。
        let expired = reading(
            99.0,
            Some(LimitHit {
                message: "old limit".into(),
                until: Some("2000-01-01T00:00:00Z".into()),
                at: "1999-12-31T00:00:00Z".into(),
                bucket: None,
            }),
        );
        sqlx::query("UPDATE quota_cache SET quota_json = ?, updated_at = ? WHERE key = ?")
            .bind(serde_json::to_string(&expired).unwrap())
            .bind(&expired.updated_at)
            .bind("claude")
            .execute(&restarted.db)
            .await
            .unwrap();
        let after_expiry = crate::testing::restart_app(&env).await;
        load_cache(&after_expiry).await.unwrap();
        assert!(after_expiry.quotas.lock().await["claude"].limit_hit.is_none());
    }

    #[test]
    fn statusline_maps() {
        let p = json!({"hook_event_name":"StatusLine","rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, Some("cc1")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 3.5);
        assert!(q.fable.is_none());
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "statusline");
        assert!(quota_from_statusline(&json!({"model": {}}), None).is_none());
    }

    #[test]
    fn statusline_picks_up_a_fable_bucket_if_it_appears() {
        let p = json!({"rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340},
            "fable":{"used_percentage":61,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, None).unwrap();
        assert_eq!(q.seven_day.unwrap().used_pct, 22.0);
        assert_eq!(q.fable.unwrap().used_pct, 61.0);
    }

    /// issue #108：排著的 prompt 記下的撞限重啟後原樣種回——撞限時刻、桶名、沒寫時間的黏著都留著；
    /// 回填之前已經進來的讀數當場校正；過期的、這一格已有更晚（或黏著）的不寫。
    #[tokio::test]
    async fn a_restored_limit_hit_keeps_its_moment_and_bucket_and_meets_the_reading_already_in() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let t = |mins: i64| crate::db::iso_at(chrono::Utc::now() + chrono::Duration::minutes(mins));
        let hit = |at: &str, until: Option<String>, bucket: Option<&str>| LimitHit {
            message: "You've hit your session limit".into(),
            until,
            at: at.into(),
            bucket: bucket.map(String::from),
        };
        let got = |key: &'static str| {
            let app = app.clone();
            async move { app.quotas.lock().await.get(key).and_then(|q| q.limit_hit.clone()) }
        };

        let original = hit(&t(-90), Some(t(120)), Some("five_hour"));
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r1", original.clone()).await);
        assert_eq!(got("claude:r1").await, Some(original.clone()), "原樣，不是「現在」撞的");
        assert_eq!(app.quotas.lock().await["claude:r1"].source, HELD_SOURCE);
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), Some(t(60)), None)).await, "已有更晚的：不蓋");
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), None, None)).await, "黏著的比任何時間都晚");
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), Some(t(600)), None)).await, "已經黏著：不蓋");
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r2", hit(&t(-90), Some(t(-1)), None)).await, "過期的不種");

        // 重啟後、回填前就進來的讀數：5 小時窗是撞限之後才開的——撞限作廢，不種。
        let reading = |resets: String| Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 3.0, resets_at: Some(resets) }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "statusline".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:r3", reading(t(5 * 60 - 1))).await;
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r3", hit(&t(-90), Some(t(120)), Some("five_hour"))).await);
        assert_eq!(got("claude:r3").await, None);
        // 窗在撞限之前就開了：照種，但到期時間收斂到那個窗的重置。
        let resets = t(30);
        set(&app, LOCAL_HOST, "claude:r4", reading(resets.clone())).await;
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r4", hit(&t(-90), Some(t(120)), Some("five_hour"))).await);
        assert_eq!(got("claude:r4").await.and_then(|h| h.until), Some(resets));
        assert_eq!(app.quotas.lock().await["claude:r4"].five_hour.as_ref().map(|w| w.used_pct), Some(3.0), "讀數不動");
    }



    /// 寫撞限要落在查詢端之後讀的那把 key：身分表還沒偵測完、又不是手寫的身分時算不準，回錯（不猜 `claude:cc0`）。
    #[tokio::test]
    async fn the_limit_key_is_not_guessed_before_the_identities_are_known() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        assert!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("cc0")).await.is_err(), "偵測之前：cc0 是不是預設帳號還不知道");
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", None).await.unwrap(), "claude", "沒有身分就是裸 kind");
        app.cfg
            .update(|c| {
                c.identities.push(crate::config::IdentityCfg { name: "hand".into(), kind: "claude".into(), host: None, env: env(&[("CLAUDE_CONFIG_DIR", "/x")]), args: vec![] });
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("hand")).await.unwrap(), "claude:hand", "手寫的身分不必等偵測");
        let cc0 = crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cc0], utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now() },
        );
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("cc0")).await.unwrap(), "claude", "偵測完：cc0 就是預設帳號");
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("nobody")).await.unwrap(), "claude:nobody", "偵測完還查不到：照舊分開");
    }
    /// #347：探測途中同名主機被換掉，舊機器的額度不能寫進去（也不能把已移除主機的 key 種回來）。
    #[tokio::test]
    async fn a_quota_reading_from_a_superseded_host_probe_is_not_published() {
        let app = crate::testing::env().await.app.clone();
        let cfg = |ssh: &str| crate::config::HostCfg { name: "build1".into(), ssh: ssh.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence = app.hosts.fence("build1").await.unwrap();
        let reading = || Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }), seven_day: None, fable: None, reset_credits: None,
            limit_hit: None, plan: None, updated_at: crate::db::now(), source: "test".into(), account: None, host: "build1".into(),
        };
        app.hosts.insert_remote_for_test(cfg("target-b")).await;
        assert!(set_fenced(&app, "build1", "codex", reading(), &fence).await.is_err());
        assert!(app.quotas.lock().await.get("build1/codex").is_none());
        let fresh = app.hosts.fence("build1").await.unwrap();
        set_fenced(&app, "build1", "codex", reading(), &fresh).await.unwrap();
        assert!(app.quotas.lock().await.get("build1/codex").is_some());
    }

    fn plain(five: Option<Window>, seven: Option<Window>, fable: Option<Window>) -> Quota {
        Quota {
            five_hour: five, seven_day: seven, fable, reset_credits: None, limit_hit: None, plan: None,
            updated_at: crate::db::now(), source: "test".into(), account: None, host: LOCAL_HOST.into(),
        }
    }

    /// 對抗式審查：沒有任何一個入口驗過百分比（claude statusLine、codex app-server、`/usage` 文字、grok 的長條都是裸 `f64`）。
    /// 超過 100、負數都照單全收，`NaN`／`inf` 更糟——`(100 - NaN).max(0)` 是 0，身分會被判成「見底」。
    /// 唯一的共同出口是 `set`：在那裡夾進 0–100，不是有限數的整格丟掉（沿用上一份）。
    #[tokio::test]
    async fn a_percentage_outside_zero_to_one_hundred_never_reaches_the_cache_as_is() {
        let app = crate::testing::env().await.app.clone();
        let w = |used: f64| Some(Window { observed_at: None, used_pct: used, resets_at: None });
        set(&app, LOCAL_HOST, "claude", plain(w(30.0), w(40.0), w(50.0))).await;
        set(&app, LOCAL_HOST, "claude", plain(w(140.0), w(-20.0), w(f64::NAN))).await;
        let got = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(got.five_hour.as_ref().unwrap().used_pct, 100.0, "超過 100 夾到 100");
        assert_eq!(got.seven_day.as_ref().unwrap().used_pct, 0.0, "負數夾到 0");
        assert_eq!(got.fable.as_ref().unwrap().used_pct, 50.0, "NaN 不是讀數：丟掉，沿用上一份的 Fable，不是判成見底");
        for bad in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            set(&app, LOCAL_HOST, "codex", plain(w(bad), w(25.0), None)).await;
            let c = app.quotas.lock().await.get("codex").cloned().unwrap();
            assert!(c.five_hour.is_none(), "{bad} 的 5h 整格丟掉");
            assert_eq!(c.seven_day.as_ref().unwrap().used_pct, 25.0);
            assert!(!c.seven_day.as_ref().unwrap().critical());
        }
    }

    /// 對抗式審查：`quota_stale` 只有開機從快取回填時才會標，執行中探測壞掉（`/usage` 格式變了、帳號登出、pane 卡住）
    /// 舊數字會一直以「新鮮」的樣子留在畫面上。快照要看年齡：沒人更新超過半小時（最慢的健康節奏是 claude `/usage` 的 10 分鐘）就標 stale，
    /// 網頁既有的「上次讀數 N 前」就會出現。
    #[tokio::test]
    async fn a_reading_nobody_refreshed_for_half_an_hour_is_reported_stale_in_the_snapshot() {
        let app = crate::testing::env().await.app.clone();
        let w = Some(Window { observed_at: None, used_pct: 10.0, resets_at: None });
        let mut old = plain(w.clone(), w.clone(), None);
        old.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(2));
        set(&app, LOCAL_HOST, "claude", old).await;
        set(&app, LOCAL_HOST, "claude:cc1", plain(w.clone(), w.clone(), None)).await;
        let mut edge = plain(w.clone(), w.clone(), None);
        edge.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(29));
        set(&app, LOCAL_HOST, "claude:cc2", edge).await;
        let snap = snapshot(&app).await;
        assert_eq!(snap["kinds"]["claude"]["stale"], json!(true), "兩小時沒更新");
        assert_eq!(snap["kinds"]["claude:cc1"]["stale"], json!(false), "剛寫的");
        assert_eq!(snap["kinds"]["claude:cc2"]["stale"], json!(false), "29 分鐘還在健康節奏的容忍內");
    }
