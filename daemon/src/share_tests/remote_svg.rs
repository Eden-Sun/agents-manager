//! #1862 C2：遠端專案的壞 SVG 提醒。原本是 `portal_remote_tests.rs` 裡唯一要 daemon 的 prompt／turns 路徑的測試
//! （其餘 27 條已在 am-share 用 `TestShareEnv` 跑）；輔助函式（client／serve／setup）是那個檔同名函式的精簡複本，只留這個測試用到的欄位。

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::config::HostCfg;
use crate::db;
use crate::share::*;
use crate::testing as tt;

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    format!("http://{addr}")
}

struct Remote {
    bot: db::Bot,
    token: String,
    outbox: PathBuf,
    base: String,
}

impl Remote {
    fn url(&self, tail: &str) -> String {
        format!("{}/s/{}/api/{tail}", self.base, self.token)
    }
}

/// 一顆在「遠端主機」上的受限分享 bot（`portal_remote_tests::setup` 的精簡版）。
async fn setup(e: &tt::Env, tag: &str) -> Remote {
    let home = crate::test_dirs::scratch_dir(&format!("rs3-{tag}"));
    let host = format!("rs3-{tag}-{}", db::ulid().to_lowercase());
    let _shell_diag = crate::share::remote_fs::test_support::local_sh_fake_with_diagnostics(&host, &home);
    let cfg = HostCfg { daemon_url: None,
        name: host.clone(),
        ssh: "fake-ssh".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test-session".into(),
        shared_session: false,
        remote_path: String::new(),
    };
    let conn = e.app.hosts.insert_remote_for_test(cfg).await;
    conn.connected.store(true, Ordering::SeqCst);
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(&host).bind(&e.project_id).execute(&e.app.db).await.unwrap();
    let bot = tt::claude_bot(&e.app, &e.project_id, tag).await;
    let workspace = home.join("shared-bots").join(tag);
    std::fs::create_dir_all(workspace.join("inbox")).unwrap();
    std::fs::set_permissions(workspace.join("inbox"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let outbox = PathBuf::from(crate::outbox_remote::remote_dir(&home.to_string_lossy(), e.app.instance().as_deref(), &bot.id).unwrap());
    std::fs::create_dir_all(&outbox).unwrap();
    store::insert_share_bot(&e.app.db, &bot.id, "restricted", &workspace.to_string_lossy()).await.unwrap();
    let token = store::enable(&e.app.db, &bot.id).await.unwrap().expect("a fresh share");
    let base = serve(portal::router(e.app.clone())).await;
    Remote { bot, token, outbox, base }
}

#[tokio::test]
async fn a_broken_remote_svg_gets_one_reminder() {
    let e = tt::env().await;
    let r = setup(&e, "svg-bad").await;
    std::fs::write(r.outbox.join("card.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\">\n<text x=\"540\" y=\"380\"font-size=\"100\">嗨</text>\n</svg>").unwrap();
    let listed = client().get(r.url("files")).send().await.unwrap();
    assert_eq!(listed.status(), 200);
    for _ in 0..2 {
        let err = crate::share::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await.expect("壞掉的檔");
        assert_eq!((err.line, err.col), (2, 22));
    }
    let reminders: Vec<(String,)> = sqlx::query_as("SELECT content FROM messages WHERE role = 'user' AND content LIKE '%card.svg%'").fetch_all(&e.app.db).await.unwrap();
    assert_eq!(reminders.len(), 1, "同一個檔同一個錯誤只提醒一次：{reminders:?}");
    std::fs::write(r.outbox.join("card.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"><text x=\"540\" y=\"380\" font-size=\"100\">嗨</text></svg>").unwrap();
    assert_eq!(crate::share::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await, None, "修好了就不再提醒");
}
