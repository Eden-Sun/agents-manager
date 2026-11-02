//! 遠端 herdr 的持久化中繼：一條長駐的 ssh 行程，stdin／stdout 逐行轉送 JSON-RPC 到遠端的 herdr socket。
//!
//! 為什麼需要：遠端的 herdr socket 經 `ssh -L`（unix socket 轉送）轉過來，每個 RPC 都是一條新連線，
//! 而 streamlocal 轉送的資料往返約 110 ms（ping 1.2 ms、同一條 ssh 主連線上的 session 也只要 20 ms；
//! 遠端 herdr 本身 0.4 ms）。這裡改走一條 `ssh <host> python3 …` 的 session，請求與回應只在這條 channel 上來回，
//! 實測 `pane.read` 中位數 1.0 ms。herdr 的 server 每條連線只收一個請求（收完就關），所以不能在 socket 上重用連線，
//! 只能在 relay 這一層重用。
//!
//! 安全界線（最重要）：送出去的請求**不能重送**。中繼一旦寫出請求、之後才失敗（讀不到回應、逾時、回應對不上），
//! 回 [`Exchange::Sent`]，呼叫端照實回錯，不會退回 socket 再送一次——否則 `pane.send_keys` 會把按鍵打兩次。
//! 只有「中繼還沒證明過能用」、或「還沒寫出請求就失敗」才回 [`Exchange::NotSent`]，那時呼叫端退回直連 socket。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// 中繼起不來或壞掉之後，這段時間內不再嘗試（否則每個 RPC 都會重開一條 ssh）。
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// 一次交換的結果。`NotSent`：請求還沒有離開這個程序，呼叫端可以安全地改走別的路（直連 socket）。
/// `Sent`：請求已寫出，結果（成功或失敗）就是答案，不能再送一次。
pub enum Exchange {
    NotSent(anyhow::Error),
    Sent(Result<String>),
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Default)]
struct State {
    proc: Option<Proc>,
    /// 中繼證明過能用（至少回過一個正確的回應）之後才為真；之前的失敗一律當成「沒送出」。
    proven: bool,
    disabled_until: Option<Instant>,
}

/// 一條中繼。`argv` 是完整的啟動命令（遠端的 ssh 命令列，或測試用的本機 python）。
pub struct Relay {
    argv: Vec<String>,
    state: Mutex<State>,
}

impl Relay {
    pub fn new(argv: Vec<String>) -> Arc<Self> {
        Arc::new(Self { argv, state: Mutex::new(State::default()) })
    }

    /// 送一行請求（已含結尾換行），等一行回應。`expect_id` 用來確認回應對得上這個請求。
    /// 中繼忙著（另一個請求還在等回應）就**不排隊**：回 `NotSent`，呼叫端直連 socket。請求還沒送出，所以不會重送。
    pub async fn exchange(&self, line: &str, expect_id: &str, timeout: Duration) -> Exchange {
        let Ok(mut st) = self.state.try_lock() else {
            return Exchange::NotSent(anyhow!("herdr relay busy"));
        };
        if st.disabled_until.is_some_and(|t| Instant::now() < t) {
            return Exchange::NotSent(anyhow!("herdr relay disabled for now"));
        }
        if st.proc.is_none() {
            match self.spawn() {
                Ok(p) => st.proc = Some(p),
                Err(e) => {
                    st.disabled_until = Some(Instant::now() + RETRY_AFTER);
                    return Exchange::NotSent(e);
                }
            }
        }
        let proc = st.proc.as_mut().expect("spawned above");
        if let Err(e) = proc.stdin.write_all(line.as_bytes()).await.and(proc.stdin.flush().await) {
            // 寫不進去：中繼已經死了，請求沒有到達遠端。
            Self::drop_proc(&mut st);
            if !st.proven {
                st.disabled_until = Some(Instant::now() + RETRY_AFTER);
            }
            return Exchange::NotSent(anyhow!("write to herdr relay: {e}"));
        }
        let mut buf = String::new();
        let read = tokio::time::timeout(timeout, proc.stdout.read_line(&mut buf)).await;
        let result = match read {
            Err(_) => Err(anyhow!("herdr relay timed out after {timeout:?}")),
            Ok(Err(e)) => Err(anyhow!("read from herdr relay: {e}")),
            Ok(Ok(0)) => Err(anyhow!("herdr relay closed without a response")),
            Ok(Ok(_)) => {
                if response_id(&buf).as_deref() == Some(expect_id) {
                    Ok(buf)
                } else {
                    Err(anyhow!("herdr relay answered out of order (expected id {expect_id})"))
                }
            }
        };
        match result {
            Ok(b) => {
                st.proven = true;
                Exchange::Sent(Ok(b))
            }
            Err(e) => {
                // 回應對不上或讀不到：這條管線的狀態不可信，丟掉重來；請求已送出，所以不退回重送。
                Self::drop_proc(&mut st);
                if !st.proven {
                    // 從來沒成功過：多半是遠端沒有 python3 之類，短暫停用，之後的呼叫直接走 socket。
                    st.disabled_until = Some(Instant::now() + RETRY_AFTER);
                    return Exchange::NotSent(e);
                }
                Exchange::Sent(Err(e))
            }
        }
    }

    fn drop_proc(st: &mut State) {
        if let Some(mut p) = st.proc.take() {
            let _ = p.child.start_kill();
        }
    }

    fn spawn(&self) -> Result<Proc> {
        let (program, args) = self.argv.split_first().ok_or_else(|| anyhow!("empty relay command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn herdr relay: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("relay stdin missing"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("relay stdout missing"))?;
        Ok(Proc { child, stdin, stdout: BufReader::new(stdout) })
    }
}

/// 短呼叫白名單：只有這些 method、而且參數裡沒有 `timeout_ms`，才走中繼。
/// 其餘（`agent.wait`、`agent.start`、`pane.wait_output`……）可能長等，一律直連 socket，免得把中繼占住、擋住同主機的其他 RPC。
pub fn is_short_call(method: &str, params: &serde_json::Value) -> bool {
    const SHORT: &[&str] = &[
        "ping",
        "pane.get",
        "pane.list",
        "pane.read",
        "pane.layout",
        "pane.process_info",
        "pane.send_text",
        "pane.send_keys",
    ];
    SHORT.contains(&method) && params.get("timeout_ms").is_none()
}

/// 回應的 `id`（不是 JSON 或沒有 id 就是 `None`）。
fn response_id(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    v.get("id")?.as_str().map(str::to_string)
}

/// 遠端 python 中繼的程式（經 base64 交給 `python3 -c`，免得 ssh 的引號問題）。
/// 每行請求開一條到 herdr socket 的連線（herdr 每條連線只收一個請求），收滿一行就寫回 stdout。
pub const REMOTE_PY: &str = r#"import socket,sys
p=sys.argv[1]
for line in sys.stdin:
    s=socket.socket(socket.AF_UNIX)
    s.connect(p)
    s.sendall(line.encode())
    buf=b""
    while not buf.endswith(b"\n"):
        c=s.recv(1<<20)
        if not c:
            break
        buf+=c
    s.close()
    sys.stdout.write(buf.decode())
    sys.stdout.flush()
"#;

/// 遠端命令：`python3 -u -c "import base64;exec(base64.b64decode('…'))" '<sock>'`。
pub fn remote_command(remote_sock: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let b64 = STANDARD.encode(REMOTE_PY);
    format!("python3 -u -c \"import base64;exec(base64.b64decode('{b64}'))\" '{}'", remote_sock.replace('\'', ""))
}

/// 給 ssh 用的完整 argv：`ssh <opts…> <target> <遠端命令>`。
pub fn ssh_argv(ssh_opts: &[String], target: &str, remote_sock: &str) -> Vec<String> {
    let mut v = vec!["ssh".to_string(), "-T".to_string()];
    v.extend(ssh_opts.iter().cloned());
    v.push(target.to_string());
    v.push(remote_command(remote_sock));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本機的假 herdr：每條連線讀一行，回一行 `{"id":…,"result":{"echo":…}}`，然後關線（跟真的 herdr 一樣）。
    fn fake_herdr(tag: &str) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
        let dir = crate::test_dirs::scratch_dir(&format!("am-relay-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let (r, mut w) = conn.split();
                    let mut line = String::new();
                    if BufReader::new(r).read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
                    let out = serde_json::json!({"id": req["id"], "result": {"echo": req["method"]}});
                    let _ = w.write_all(format!("{out}\n").as_bytes()).await;
                });
            }
        });
        (sock, h)
    }

    /// 本機用 python 跑的中繼（跟遠端那段同一份程式），對假 herdr 說話。
    fn local_relay(sock: &std::path::Path) -> Arc<Relay> {
        Relay::new(vec![
            "python3".into(),
            "-u".into(),
            "-c".into(),
            REMOTE_PY.into(),
            sock.display().to_string(),
        ])
    }

    fn req(id: &str, method: &str) -> String {
        format!("{}\n", serde_json::json!({"id": id, "method": method, "params": {}}))
    }

    /// 同一條中繼連續送多個請求，每個都拿到自己的回應（沒有重連、沒有錯位）。
    #[tokio::test]
    async fn one_relay_carries_many_requests_in_order() {
        let (sock, server) = fake_herdr("many");
        let relay = local_relay(&sock);
        for i in 0..20 {
            let id = format!("r{i}");
            let got = relay.exchange(&req(&id, "pane.read"), &id, Duration::from_secs(5)).await;
            let Exchange::Sent(Ok(line)) = got else { panic!("request {i} did not get an answer") };
            assert_eq!(response_id(&line).as_deref(), Some(id.as_str()));
        }
        server.abort();
    }

    /// 中繼起不來：回 `NotSent`（呼叫端會退回 socket），之後短時間內不再重開。
    #[tokio::test]
    async fn a_relay_that_cannot_start_is_not_sent_and_paused() {
        let relay = Relay::new(vec!["/nonexistent/herdr-relay-binary".into()]);
        assert!(matches!(relay.exchange(&req("a", "ping"), "a", Duration::from_secs(1)).await, Exchange::NotSent(_)));
        let started = Instant::now();
        assert!(matches!(relay.exchange(&req("b", "ping"), "b", Duration::from_secs(1)).await, Exchange::NotSent(_)));
        assert!(started.elapsed() < Duration::from_millis(500), "停用期間不該再試著開行程");
    }

    /// 中繼從來沒成功過、回應就壞掉（不是我們要的 id）：算沒送出，可以退回 socket。
    #[tokio::test]
    async fn a_first_bad_answer_is_not_sent_so_the_caller_can_fall_back() {
        // 回的不是 JSON（沒有 id）：中繼沒證明過能用，所以算沒送出。
        let relay = Relay::new(vec!["sh".into(), "-c".into(), "while read l; do echo not-json; done".into()]);
        assert!(matches!(relay.exchange(&req("c", "ping"), "c", Duration::from_secs(2)).await, Exchange::NotSent(_)));
    }

    #[test]
    fn the_remote_command_carries_the_socket_and_a_base64_script() {
        let cmd = remote_command("/Users/m4p/.config/herdr/sessions/agents-manager/herdr.sock");
        assert!(cmd.starts_with("python3 -u -c "));
        assert!(cmd.ends_with("'/Users/m4p/.config/herdr/sessions/agents-manager/herdr.sock'"));
        assert!(!cmd.contains("import socket"), "腳本是 base64，不該明文出現在命令列");
    }

    /// 掛上中繼的 client：RPC 走中繼，自己的 socket 路徑根本不存在也沒關係。
    #[tokio::test]
    async fn a_client_with_a_relay_answers_through_it() {
        let (sock, server) = fake_herdr("client");
        let client = crate::herdr::HerdrClient::new("/nonexistent/herdr.sock");
        client.set_relay(Some(local_relay(&sock)));
        let v = client.call("pane.read", serde_json::json!({})).await.expect("answered via the relay");
        assert_eq!(v["echo"], "pane.read");
        server.abort();
    }

    /// 中繼起不來：client 退回直連 socket，呼叫照樣成功。
    #[tokio::test]
    async fn a_client_falls_back_to_its_socket_when_the_relay_cannot_start() {
        let (sock, server) = fake_herdr("fallback");
        let client = crate::herdr::HerdrClient::new(&sock);
        client.set_relay(Some(Relay::new(vec!["/nonexistent/herdr-relay-binary".into()])));
        let v = client.call("ping", serde_json::json!({})).await.expect("answered via the socket");
        assert_eq!(v["echo"], "ping");
        server.abort();
    }

    #[test]
    fn only_short_calls_use_the_relay() {
        use serde_json::json;
        assert!(is_short_call("pane.read", &json!({"pane_id": "w1:p1"})));
        assert!(is_short_call("pane.send_keys", &json!({"keys": ["enter"]})));
        assert!(!is_short_call("agent.wait", &json!({"timeout_ms": 5000})), "長等不走中繼");
        assert!(!is_short_call("agent.start", &json!({"timeout_ms": 30000})));
        assert!(!is_short_call("pane.read", &json!({"timeout_ms": 1})), "帶 timeout_ms 的一律直連");
        assert!(!is_short_call("pane.wait_output", &json!({"pane_id": "w1:p1"})), "不在白名單就直連");
    }

    /// 長呼叫（帶 timeout_ms）不經中繼：中繼就算壞掉或卡住，這條呼叫照樣直連 socket 成功。
    #[tokio::test]
    async fn a_long_call_does_not_touch_the_relay() {
        let (sock, server) = fake_herdr("long");
        let client = crate::herdr::HerdrClient::new(&sock);
        // 會卡住的中繼：一行回應都不給。若長呼叫走了它，這裡會等到逾時。
        client.set_relay(Some(Relay::new(vec!["sh".into(), "-c".into(), "cat > /dev/null".into()])));
        let v = tokio::time::timeout(Duration::from_secs(5), client.call("agent.wait", serde_json::json!({"timeout_ms": 1000})))
            .await
            .expect("長呼叫不該等中繼")
            .expect("answered via socket");
        assert_eq!(v["echo"], "agent.wait");
        server.abort();
    }

    /// 中繼忙碌（另一個請求還佔著）時，短呼叫不排隊，直接退回 socket 完成。
    #[tokio::test]
    async fn a_busy_relay_is_skipped_not_queued() {
        let (sock, server) = fake_herdr("busy");
        let relay = Relay::new(vec!["sh".into(), "-c".into(), "cat > /dev/null".into()]);
        let client = crate::herdr::HerdrClient::new(&sock);
        client.set_relay(Some(relay.clone()));
        // 另一個任務持有中繼（模擬一個正在等回應的請求）。
        let holder = relay.clone();
        let hold = tokio::spawn(async move {
            let _g = holder.state.lock().await;
            tokio::time::sleep(Duration::from_secs(20)).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let v = tokio::time::timeout(Duration::from_secs(3), client.call("pane.get", serde_json::json!({"pane_id": "x"})))
            .await
            .expect("忙碌時不排隊，該立刻走 socket")
            .expect("answered via socket");
        assert_eq!(v["echo"], "pane.get");
        hold.abort();
        server.abort();
    }
}
