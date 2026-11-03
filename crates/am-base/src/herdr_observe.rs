//! herdr 0.9.5 的 `terminal session observe` 串流：真正的畫面推送（每幀是 ANSI 位元組，只送差異）。
//!
//! 為什麼是子行程、不是 socket RPC：observe 走的是 **client socket** 的二進位（bincode）握手與訊框，
//! 不是 API socket 上的 JSON-RPC（`events.subscribe` 那種）。自己重做一份協定太脆，所以這裡起
//! `herdr terminal session observe <pane> --cols C --rows R` 這個 CLI 子行程，逐行讀它 stdout 的 JSON：
//!
//! ```text
//! {"type":"terminal.frame","seq":N,"encoding":"ansi","width":W,"height":H,"full":bool,"bytes":"<base64 ANSI>"}
//! {"type":"terminal.closed","reason":"…"}
//! ```
//!
//! 本機：`HERDR_SOCKET_PATH` 指到這個 client 的 API socket（CLI 自己由它推出 client socket）。
//! 遠端：經 ssh（同一條主連線）在遠端跑同一支 CLI，`HERDR_SOCKET_PATH` 指遠端的 API socket。
//! 子行程用 `kill_on_drop`：串流的 owner 丟掉就會被殺，不會留孤兒。

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{ChildStdout, Command};

/// 要起的子行程（程式、參數、額外環境變數）。與 I/O 分開，好測組出來的 argv。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveCmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// 一行 observe 輸出解析後的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveMsg {
    /// 一幀：`bytes` 已經從 base64 解成原始 ANSI 位元組；`full` 為真時 client 應先 reset 再寫。
    Frame { seq: u64, full: bool, width: u32, height: u32, bytes: Vec<u8> },
    /// herdr 主動關閉（伺服器關機等）。
    Closed { reason: String },
}

/// 解析一行 CLI 輸出。不認得的型別、壞掉的 JSON、壞掉的 base64 都回 `None`（忽略，不中斷串流）。
pub fn parse_line(line: &str) -> Option<ObserveMsg> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("type")?.as_str()? {
        "terminal.frame" => {
            if v.get("encoding").and_then(Value::as_str).is_some_and(|e| e != "ansi") {
                return None;
            }
            let bytes = STANDARD.decode(v.get("bytes")?.as_str()?).ok()?;
            Some(ObserveMsg::Frame {
                seq: v.get("seq")?.as_u64()?,
                full: v.get("full")?.as_bool()?,
                width: v.get("width")?.as_u64()? as u32,
                height: v.get("height")?.as_u64()? as u32,
                bytes,
            })
        }
        "terminal.closed" => Some(ObserveMsg::Closed { reason: v.get("reason").and_then(Value::as_str).unwrap_or("").to_string() }),
        _ => None,
    }
}

/// 單引號包起來給遠端 `sh -c` 用（pane id 與數字本來就安全，這裡仍一律包，免得以後放寬時出事）。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 遠端要跑的那一行 shell：先接上 PATH 前綴，再用 `env` 指定遠端 API socket 去跑 CLI。
pub fn remote_script(path_prefix: &str, remote_sock: &str, pane: &str, cols: u32, rows: u32) -> String {
    format!(
        "{path_prefix}exec env HERDR_SOCKET_PATH={} herdr terminal session observe {} --cols {cols} --rows {rows}",
        sh_quote(remote_sock),
        sh_quote(pane)
    )
}

/// 一條進行中的 observe 串流。`next` 讀下一個 frame；EOF 或子行程結束回 `None`。
pub struct ObserveStream {
    child: tokio::process::Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl ObserveStream {
    /// 起子行程。stdin 關掉（只讀），stderr 丟掉（失敗由退出碼與「沒收到任何 frame」判斷）。
    pub fn start(cmd: &ObserveCmd) -> std::io::Result<Self> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .envs(cmd.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = c.spawn()?;
        let stdout = child.stdout.take().expect("stdout 是 piped");
        Ok(ObserveStream { child, lines: BufReader::new(stdout).lines() })
    }

    /// 下一則可解析的訊息。不認得的行直接跳過；EOF 回 `None`。
    pub async fn next(&mut self) -> Option<ObserveMsg> {
        loop {
            match self.lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(m) = parse_line(&line) {
                        return Some(m);
                    }
                }
                _ => return None,
            }
        }
    }

    /// 等子行程結束（EOF 之後呼叫，取退出碼）。
    pub async fn exit_status(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }
}

/// 組出本機 CLI 的 argv。`program` 預設 `herdr`（PATH 上的真 CLI；bot 的 shim 只在 bot pane 裡）。
pub fn local_cmd(program: &str, socket: &std::path::Path, pane: &str, cols: u32, rows: u32) -> ObserveCmd {
    ObserveCmd {
        program: program.to_string(),
        args: vec![
            "terminal".into(),
            "session".into(),
            "observe".into(),
            pane.into(),
            "--cols".into(),
            cols.to_string(),
            "--rows".into(),
            rows.to_string(),
        ],
        env: vec![("HERDR_SOCKET_PATH".into(), socket.display().to_string())],
    }
}

/// 組出遠端（經 ssh）的 argv：`ssh -T <opts…> <target> <遠端 shell 行>`。
pub fn remote_cmd(ssh_opts: &[String], target: &str, script: &str) -> ObserveCmd {
    let mut args = vec!["-T".to_string()];
    args.extend(ssh_opts.iter().cloned());
    args.push(target.to_string());
    args.push(script.to_string());
    ObserveCmd { program: "ssh".into(), args, env: vec![] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_frame_line_decodes_bytes_and_keeps_the_flags() {
        let line = r#"{"bytes":"G1sySg==","encoding":"ansi","full":true,"height":24,"seq":1,"type":"terminal.frame","width":80}"#;
        assert_eq!(
            parse_line(line),
            Some(ObserveMsg::Frame { seq: 1, full: true, width: 80, height: 24, bytes: b"\x1b[2J".to_vec() })
        );
    }

    #[test]
    fn a_delta_frame_is_not_full() {
        let line = r#"{"type":"terminal.frame","seq":7,"encoding":"ansi","width":120,"height":40,"full":false,"bytes":"aGk="}"#;
        assert_eq!(parse_line(line), Some(ObserveMsg::Frame { seq: 7, full: false, width: 120, height: 40, bytes: b"hi".to_vec() }));
    }

    #[test]
    fn closed_and_garbage_lines() {
        assert_eq!(parse_line(r#"{"type":"terminal.closed","reason":"server shutdown"}"#), Some(ObserveMsg::Closed { reason: "server shutdown".into() }));
        assert_eq!(parse_line("not json"), None, "壞 JSON 忽略");
        assert_eq!(parse_line(r#"{"type":"terminal.frame","seq":1,"full":true,"width":1,"height":1,"bytes":"!!!"}"#), None, "壞 base64 忽略");
        assert_eq!(parse_line(r#"{"type":"something.new"}"#), None, "不認得的型別忽略");
        assert_eq!(parse_line(r#"{"type":"terminal.frame","seq":1,"encoding":"kitty","full":true,"width":1,"height":1,"bytes":""}"#), None, "非 ansi 編碼忽略");
    }

    #[test]
    fn local_argv_targets_the_pane_and_the_socket() {
        let c = local_cmd("herdr", std::path::Path::new("/run/u/herdr.sock"), "w17:p5X", 100, 30);
        assert_eq!(c.program, "herdr");
        assert_eq!(c.args, vec!["terminal", "session", "observe", "w17:p5X", "--cols", "100", "--rows", "30"]);
        assert_eq!(c.env, vec![("HERDR_SOCKET_PATH".to_string(), "/run/u/herdr.sock".to_string())]);
    }

    #[test]
    fn remote_argv_runs_the_same_cli_over_ssh_with_the_remote_socket() {
        let script = remote_script("export PATH='/opt/bin':\"$PATH\"; ", "/home/u/.config/herdr/herdr.sock", "w2:p1", 80, 24);
        let c = remote_cmd(&["-S".into(), "/tmp/ctl".into()], "m4p", &script);
        assert_eq!(c.program, "ssh");
        assert_eq!(&c.args[..4], &["-T", "-S", "/tmp/ctl", "m4p"]);
        assert!(c.args[4].contains("HERDR_SOCKET_PATH='/home/u/.config/herdr/herdr.sock' herdr terminal session observe 'w2:p1' --cols 80 --rows 24"), "{}", c.args[4]);
        assert!(c.env.is_empty());
    }

    #[test]
    fn sh_quote_survives_a_single_quote() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    /// 實測用（不在 CI 跑）：`cargo test -p am-base measure_send_to_frame -- --ignored --nocapture`。
    /// 環境變數：`AM_OBS_SOCK`＝本機 API socket、`AM_OBS_PANE`＝測試 pane（要是 shell）；
    /// 量遠端再加 `AM_OBS_SSH_TARGET`（ssh 目標，已能免密登入）、`AM_OBS_REMOTE_SOCK`（遠端 API socket）、
    /// `AM_OBS_SSH_OPTS`（空白分隔的 ssh 選項，例如 `-S /path/ctl`）、`AM_OBS_REMOTE_PREFIX`（遠端 PATH 前綴，可空）。
    /// 每次：`send_text("x")` 之後量到第一幀的時間，再 `send_keys BSpace` 清掉（同樣量）。
    #[tokio::test]
    #[ignore]
    async fn measure_send_to_frame_latency() {
        use std::time::Instant;
        let sock = std::env::var("AM_OBS_SOCK").expect("AM_OBS_SOCK");
        let pane = std::env::var("AM_OBS_PANE").expect("AM_OBS_PANE");
        let client = crate::herdr::HerdrClient::new(sock);
        if let Ok(target) = std::env::var("AM_OBS_SSH_TARGET") {
            // 遠端：送字走中繼、畫面走 ssh 起的 observe，與 hosts 連上後設的一樣。
            let ssh_opts: Vec<String> = std::env::var("AM_OBS_SSH_OPTS").unwrap_or_default().split_whitespace().map(str::to_string).collect();
            let remote_sock = std::env::var("AM_OBS_REMOTE_SOCK").expect("AM_OBS_REMOTE_SOCK");
            let mut relay_opts = ssh_opts.clone();
            relay_opts.push("-o".into());
            relay_opts.push("BatchMode=yes".into());
            client.set_relay(Some(crate::herdr_relay::Relay::new(crate::herdr_relay::ssh_argv(&relay_opts, &target, &remote_sock))));
            client.set_observe_remote(Some(crate::herdr::RemoteObserve {
                ssh_opts: relay_opts,
                target,
                remote_sock,
                path_prefix: std::env::var("AM_OBS_REMOTE_PREFIX").unwrap_or_default(),
            }));
        }
        let mut stream = ObserveStream::start(&client.observe_cmd(&pane, 80, 24)).expect("起 observe");
        let first = tokio::time::timeout(Duration::from_secs(20), stream.next()).await.expect("20 秒內要有第一幀");
        println!("first frame: {:?}", first.map(|m| matches!(m, ObserveMsg::Frame { full: true, .. })));
        // 讓 shell 的提示字元與開機訊息靜下來，免得量到的是背景輸出。
        loop {
            if tokio::time::timeout(Duration::from_millis(800), stream.next()).await.is_err() {
                break;
            }
        }
        let mut lat_ms = Vec::new();
        for i in 0..30 {
            let (key, text): (&str, Option<&str>) = if i % 2 == 0 { ("", Some("x")) } else { ("Backspace", None) };
            // 送出的同時就開始計時；之後第一個進來的幀就是這次的回應。
            let t0 = Instant::now();
            match text {
                Some(t) => client.pane_send_text(&pane, t).await.expect("send_text"),
                None => client.pane_send_keys(&pane, &[key]).await.expect("send_keys"),
            }
            let got = loop {
                match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                    Ok(Some(ObserveMsg::Frame { .. })) => break Some(t0.elapsed()),
                    Ok(Some(_)) => continue,
                    _ => break None,
                }
            };
            match got {
                Some(d) => lat_ms.push(d.as_secs_f64() * 1000.0),
                None => println!("iteration {i}: no frame within 5s"),
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = lat_ms.len();
        let pct = |p: f64| lat_ms[((n as f64 - 1.0) * p).round() as usize];
        println!("send→frame over {n} sends: min {:.1} ms · p50 {:.1} ms · p90 {:.1} ms · max {:.1} ms", lat_ms[0], pct(0.5), pct(0.9), lat_ms[n - 1]);
    }
}
