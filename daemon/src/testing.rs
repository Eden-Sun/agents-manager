//! Shared test fixtures: a mock herdr on a real unix socket, a daemon `App` over a throw-away
//! sqlite file and git repository, and a fake running `runs` row.
use crate::db;
use crate::state::App;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// Mock herdr on a real unix socket, one newline-JSON request per connection. Hook injection is
/// deliberately left to a live agent. `ensure_kind_installed` 不看機器的 PATH：測試 build 的
/// `App.kind_probe` 預設答「有」（`kind_probe.rs`；要走真的探測就自己 `set` 一個 runner）。
///
/// Knowingly differs from herdr 0.8.2: closing a tab's last pane does **not** reap the tab, so tests
/// can show the daemon tidies the tab up itself.
#[derive(Debug, Clone)]
pub struct MockTab {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
    pub panes: Vec<String>,
}

pub struct MockHerdr {
    #[allow(dead_code)]
    pub workspaces: Arc<StdMutex<BTreeMap<String, String>>>,
    pub tabs: Arc<StdMutex<Vec<MockTab>>>,
    /// `pane.read` answers, per pane id.
    pub screens: Arc<StdMutex<BTreeMap<String, String>>>,
    /// `pane.read` revisions for static screens; race tests can bump a revision without changing text.
    screen_revisions: Arc<StdMutex<BTreeMap<String, u64>>>,
    /// `pane.process_info` argv, per pane id.
    pub argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
    /// `pane.process_info` pid (default 1); only tests reading the process's account (SPEC §16.6) need it.
    pub pids: Arc<StdMutex<BTreeMap<String, i64>>>,
    /// `pane.process_info` 的 `shell_pid`，per pane id（沒設就不回，像讀不到的 herdr）。
    pub shell_pids: Arc<StdMutex<BTreeMap<String, i64>>>,
    /// Every `(method, params)` sent, so a test can assert *how* the daemon asked.
    pub calls: Arc<StdMutex<Vec<(String, Value)>>>,
    /// Panes that behave like a TUI: typing lands in a composer, Enter moves it to the transcript.
    pub live: Arc<StdMutex<BTreeMap<String, LivePane>>>,
    /// Answer `pane.read` with `format: ansi` like a herdr that does not know the parameter.
    pub reject_ansi: Arc<std::sync::atomic::AtomicBool>,
    /// Accept `format: ansi` but answer `format: text`, like a herdr that ignores unknown params.
    pub ignore_ansi: Arc<std::sync::atomic::AtomicBool>,
    /// What `agent.list` and `agent.get` answer with.
    pub agents: Arc<StdMutex<Vec<Value>>>,
    /// 下一次（或下幾次）呼叫某個方法時要出的狀況，見 [`MockHerdr::fail_next`]。
    faults: Arc<StdMutex<Vec<(String, Fault)>>>,
    /// 先讓這個方法成功 `skip` 次，再套用 fault（#647 分段貼上的第 2 段）。
    defer: Arc<StdMutex<Vec<(String, usize, Fault)>>>,
    /// `agent.list` 回空陣列，但 `agent.get` 仍看得到 `agents`：模擬「清單暫時是空的、agent 其實還在」。
    pub hide_agent_list: Arc<std::sync::atomic::AtomicBool>,
    /// `events.subscribe` 回 ack 並把連線留著（不吐事件）。預設關：沒開時訂閱照舊回 `unsupported`，既有測試依賴它「訂閱不起來」。
    pub allow_subscribe: Arc<std::sync::atomic::AtomicBool>,
    /// `ping` 的回答 `(version, protocol)`；測 live-handoff 後版本變了（#254）。
    pub pong: Arc<StdMutex<(String, u32)>>,
    handle: tokio::task::JoinHandle<()>,
}

/// 一次 RPC 可以怎麼壞（#120／#147）：三種壞法對呼叫端意思不同——
/// herdr 明確拒絕＝什麼都沒做；連線斷了沒回＝不知道做了沒有，而且真的可能兩種都有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// herdr 回錯誤，什麼都不做（像 `pane_not_found`）。
    Refuse,
    /// 同 `Refuse`，但錯誤碼是指定的那個（像 `agent_blocked`，#149）。
    RefuseWith(&'static str),
    /// 收到請求就斷線，什麼都沒做、也沒回。
    DropBefore,
    /// 照做了（字進框、鍵按下去）才斷線，沒回。
    DropAfter,
}

impl Drop for MockHerdr {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A pane that reacts: `pane.send_text` fills the composer, Enter moves it into the transcript,
/// `ctrl+c` clears it, and every read redraws a spinner row so "the screen changed" is worthless
/// as evidence on its own (sol review 2026-09-14 #1).
#[derive(Clone, Default)]
pub struct LivePane {
    pub transcript: Vec<String>,
    pub composer: Vec<String>,
    pub reads: u32,
    /// Revision returned by fake `pane.read`; race tests can bump it without changing composer text.
    pub revision: u64,
    /// A TUI that eats the Enter: the text stays in the box.
    pub swallow_enter: bool,
    /// A TUI that throws the typed text away without submitting it (the 2026-09-14 incident).
    pub swallow_text: bool,
    /// Like claude's session transcript: every submitted message is appended as a user entry.
    pub transcript_file: Option<std::path::PathBuf>,
    /// Columns `pane.layout` reports for this pane; `None` answers like herdr without a layout.
    pub width: Option<u32>,
    /// Draw grok's boxed composer (`│ ❯ … │` over `╰── Grok 4.6 (low) ─╯`) instead of claude's.
    pub boxed: bool,
    /// Draw codex's unboxed `› …` composer and echo rows instead of claude's.
    pub codex: bool,
    /// claude's suggested next prompt: drawn dim in an empty composer, gone once anything is typed.
    pub suggestion: Option<String>,
    /// claude 2.1.277+（#205）：Enter 時框裡有這些字就先拿掉、清過的字留在框裡，底下顯示 review 提示；再按一次才送出。
    pub strips_on_enter: Option<fn(char) -> bool>,
    /// 上面那個 review 提示（畫在框的下緣之後）。
    pub notice: Option<String>,
    /// 真的 herdr 0.9.1＋claude（2026-09-21 實測，#382）：一次 `pane.send_text` 超過這麼多位元組時，
    /// **最前面的這麼多位元組不見了**，只剩後面的進到框裡。`Some(1024)` 重現那個行為。
    pub send_text_drops_head_over: Option<usize>,
    /// 畫面列數（`pane.get` 的 `scroll.viewport_rows`）。有值時 claude 的框照真 claude 2.1.280 的樣子
    /// 最多畫 `rows/2 - 5` 列、只留最後幾列（#403）；`None` 時整段都畫、`pane.get` 不回 `scroll`。
    pub rows: Option<u32>,
}

/// 框裡的字送出去：進 transcript，有 transcript 檔就照 claude 的格式補一筆 user entry。
/// Enter 與 send-now 和弦（issue #103）共用這一段——兩顆鍵對框的效果一樣。
fn submit_composer(p: &mut LivePane) {
    if let Some(strips) = p.strips_on_enter {
        let removed: usize = p.composer.iter().map(|r| r.chars().filter(|c| strips(*c)).count()).sum();
        if removed > 0 {
            for r in p.composer.iter_mut() {
                r.retain(|c| !strips(c));
            }
            let what = if removed == 1 { "Removed 1 invisible character".to_string() } else { format!("Removed {removed} invisible characters") };
            p.notice = Some(format!("{what} · review and press Enter to send"));
            return;
        }
    }
    p.notice = None;
    let rows: Vec<String> = p.composer.drain(..).collect();
    if let (Some(path), false) = (&p.transcript_file, rows.is_empty()) {
        use std::io::Write as _;
        let entry = json!({"type": "user", "message": {"role": "user", "content": rows.join("\n")}});
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{entry}");
        }
    }
    if let Some((first, rest)) = rows.split_first() {
        p.transcript.push(format!("❯ {first}"));
        for r in rest {
            p.transcript.push(format!("  {r}"));
        }
    }
}

impl LivePane {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.codex {
            for line in &self.transcript {
                out.push_str(&line.replacen("❯ ", "› ", 1));
                out.push('\n');
            }
            out.push('\n');
            match self.composer.split_first() {
                None => out.push_str("›\n"),
                Some((first, rest)) => {
                    out.push_str(&format!("› {first}\n"));
                    for r in rest {
                        out.push_str(&format!("  {r}\n"));
                    }
                }
            }
            out.push_str("\ngpt-6-astra low · ~/proj · Context 3% used\n");
            return out;
        }
        for line in &self.transcript {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!("✻ Crunching… ({}s · esc to interrupt)\n", self.reads));
        if self.boxed {
            out.push_str("  ╭──────────────────────────────────────────╮\n");
            match self.composer.split_first() {
                None => out.push_str("  │ ❯                                        │\n"),
                Some((first, rest)) => {
                    out.push_str(&format!("  │ ❯ {first:<40} │\n"));
                    for r in rest {
                        out.push_str(&format!("  │   {r:<40} │\n"));
                    }
                }
            }
            out.push_str("  ╰──────────────── Grok 4.6 (low) · always-approve ─╯\n");
            return out;
        }
        out.push_str("─────────────────────────────────────────────\n");
        match self.composer.split_first() {
            None => match &self.suggestion {
                Some(s) => out.push_str(&format!("❯\u{a0}\u{1b}[2m{s}\u{1b}[0m\n")),
                None => out.push_str("❯\n"),
            },
            Some(_) => {
                let max = self.rows.map_or(usize::MAX, |r| crate::lifecycle::paste_check::claude_box_max_rows(r as usize).max(1));
                let shown = &self.composer[self.composer.len().saturating_sub(max)..];
                for (n, r) in shown.iter().enumerate() {
                    out.push_str(&format!("{}{r}\n", if n == 0 { "❯ " } else { "  " }));
                }
            }
        }
        out.push_str("─────────────────────────────────────────────\n");
        if let Some(n) = &self.notice {
            out.push_str(&format!("  {n}\n"));
        }
        out.push_str("  user. | proj | OP5 61% | 5h:53%(rst 2h 35m) | 7d:95%\n");
        out
    }
}

#[derive(Clone)]
struct MockState {
    workspaces: Arc<StdMutex<BTreeMap<String, String>>>,
    tabs: Arc<StdMutex<Vec<MockTab>>>,
    calls: Arc<StdMutex<Vec<(String, Value)>>>,
    agents: Arc<StdMutex<Vec<Value>>>,
    screens: Arc<StdMutex<BTreeMap<String, String>>>,
    screen_revisions: Arc<StdMutex<BTreeMap<String, u64>>>,
    live: Arc<StdMutex<BTreeMap<String, LivePane>>>,
    reject_ansi: Arc<std::sync::atomic::AtomicBool>,
    ignore_ansi: Arc<std::sync::atomic::AtomicBool>,
    argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
    pids: Arc<StdMutex<BTreeMap<String, i64>>>,
    shell_pids: Arc<StdMutex<BTreeMap<String, i64>>>,
    faults: Arc<StdMutex<Vec<(String, Fault)>>>,
    defer: Arc<StdMutex<Vec<(String, usize, Fault)>>>,
    hide_agent_list: Arc<std::sync::atomic::AtomicBool>,
    allow_subscribe: Arc<std::sync::atomic::AtomicBool>,
    pong: Arc<StdMutex<(String, u32)>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
}

impl MockState {
    fn next(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
    fn pane_json(&self, pane_id: &str, tab: &MockTab, cwd: Option<&Value>) -> Value {
        // Named or not (like herdr after clearing a name), the occupant is the pane's agent.
        let occupant = self
            .agents
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.get("pane_id").and_then(Value::as_str) == Some(pane_id))
            .cloned();
        let (agent, status) = match occupant {
            Some(a) => (a.get("agent").cloned().unwrap_or(Value::Null), a.get("agent_status").cloned().unwrap_or(Value::Null)),
            None => (Value::Null, Value::Null),
        };
        let mut v = json!({"pane_id": pane_id, "workspace_id": tab.workspace_id, "tab_id": tab.tab_id,
               "cwd": cwd.cloned().unwrap_or(Value::Null), "agent": agent, "agent_status": status});
        if let Some(rows) = self.live.lock().unwrap().get(pane_id).and_then(|p| p.rows) {
            v["scroll"] = json!({"offset_from_bottom": 0, "max_offset_from_bottom": 0, "viewport_rows": rows});
        }
        v
    }
    fn tab_json(t: &MockTab) -> Value {
        json!({"tab_id": t.tab_id, "workspace_id": t.workspace_id, "label": t.label,
               "pane_count": t.panes.len()})
    }
    fn new_tab(&self, workspace_id: &str, label: &str) -> (MockTab, String) {
        let n = self.next();
        let tab = MockTab {
            tab_id: format!("{workspace_id}:t{n}"),
            workspace_id: workspace_id.to_string(),
            label: label.to_string(),
            panes: vec![format!("{workspace_id}:p{n}")],
        };
        let pane = tab.panes[0].clone();
        self.tabs.lock().unwrap().push(tab.clone());
        (tab, pane)
    }
    fn find_pane(&self, pane_id: &str) -> Option<MockTab> {
        self.tabs.lock().unwrap().iter().find(|t| t.panes.iter().any(|p| p == pane_id)).cloned()
    }
}

impl MockHerdr {
    pub fn start(socket: std::path::PathBuf) -> MockHerdr {
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind mock herdr socket");
        let state = MockState {
            workspaces: Default::default(),
            tabs: Default::default(),
            calls: Default::default(),
            agents: Default::default(),
            screens: Default::default(),
            screen_revisions: Default::default(),
            live: Default::default(),
            reject_ansi: Default::default(),
            ignore_ansi: Default::default(),
            argvs: Default::default(),
            pids: Default::default(),
            shell_pids: Default::default(),
            faults: Default::default(),
            defer: Default::default(),
            hide_agent_list: Default::default(),
            allow_subscribe: Default::default(),
            pong: Arc::new(StdMutex::new(("mock".into(), 20))),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        };
        let (workspaces, tabs, calls, agents) =
            (state.workspaces.clone(), state.tabs.clone(), state.calls.clone(), state.agents.clone());
        let (screens, screen_revisions, argvs, pids, shell_pids) = (
            state.screens.clone(),
            state.screen_revisions.clone(),
            state.argvs.clone(),
            state.pids.clone(),
            state.shell_pids.clone(),
        );
        let live = state.live.clone();
        let reject_ansi = state.reject_ansi.clone();
        let ignore_ansi = state.ignore_ansi.clone();
        let faults = state.faults.clone();
        let defer = state.defer.clone();
        let hide_agent_list = state.hide_agent_list.clone();
        let allow_subscribe = state.allow_subscribe.clone();
        let pong = state.pong.clone();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let st = state.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                    let (r, mut w) = stream.into_split();
                    let mut line = String::new();
                    if BufReader::new(r).read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                    let params = req.get("params").cloned().unwrap_or(json!({}));
                    st.calls.lock().unwrap().push((method.clone(), params.clone()));
                    {
                        let mut d = st.defer.lock().unwrap();
                        if let Some(i) = d.iter().position(|(m, _, _)| m == &method) {
                            if d[i].1 == 0 {
                                let (_, _, fault) = d.remove(i);
                                st.faults.lock().unwrap().insert(0, (method.clone(), fault));
                            } else {
                                d[i].1 -= 1;
                            }
                        }
                    }
                    let fault = {
                        let mut f = st.faults.lock().unwrap();
                        f.iter().position(|(m, _)| *m == method).map(|i| f.remove(i).1)
                    };
                    match fault {
                        Some(Fault::DropBefore) => return,
                        Some(f @ (Fault::Refuse | Fault::RefuseWith(_))) => {
                            let code = if let Fault::RefuseWith(c) = f { c } else { "injected_refusal" };
                            let out = json!({"id": id, "error": {"code": code, "message": format!("mock herdr refused {method}")}});
                            let mut bytes = serde_json::to_vec(&out).unwrap();
                            bytes.push(b'\n');
                            let _ = w.write_all(&bytes).await;
                            let _ = w.flush().await;
                            return;
                        }
                        Some(Fault::DropAfter) | None => {}
                    }
                    let wid_of = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    let out = match method.as_str() {
                        "ping" => {
                            let (v, p) = st.pong.lock().unwrap().clone();
                            json!({"id": id, "result": {"version": v, "protocol": p}})
                        }
                        "workspace.create" => {
                            let wid = format!("ws-{}", st.next());
                            let label = params.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                            st.workspaces.lock().unwrap().insert(wid.clone(), label.clone());
                            let (tab, pane) = st.new_tab(&wid, &label);
                            json!({"id": id, "result": {
                                "workspace": {"workspace_id": wid, "label": label, "pane_count": 1},
                                "tab": MockState::tab_json(&tab),
                                "root_pane": st.pane_json(&pane, &tab, params.get("cwd"))}})
                        }
                        "workspace.get" => {
                            let wid = wid_of("workspace_id");
                            match st.workspaces.lock().unwrap().get(&wid).cloned() {
                                Some(label) => json!({"id": id, "result": {"workspace":
                                    {"workspace_id": wid, "label": label, "pane_count": 1}}}),
                                None => json!({"id": id, "error": {"code": "not_found", "message": "no such workspace"}}),
                            }
                        }
                        "workspace.list" => {
                            let wss: Vec<Value> = st
                                .workspaces
                                .lock()
                                .unwrap()
                                .iter()
                                .map(|(w, l)| json!({"workspace_id": w, "label": l, "pane_count": 1}))
                                .collect();
                            json!({"id": id, "result": {"type": "workspace_list", "workspaces": wss}})
                        }
                        "workspace.close" => {
                            let wid = wid_of("workspace_id");
                            st.workspaces.lock().unwrap().remove(&wid);
                            st.tabs.lock().unwrap().retain(|t| t.workspace_id != wid);
                            json!({"id": id, "result": {}})
                        }
                        "tab.create" => {
                            let wid = wid_of("workspace_id");
                            let label = params.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                            if !st.workspaces.lock().unwrap().contains_key(&wid) {
                                json!({"id": id, "error": {"code": "workspace_not_found", "message": wid}})
                            } else {
                                let (tab, pane) = st.new_tab(&wid, &label);
                                json!({"id": id, "result": {"type": "tab_created",
                                    "tab": MockState::tab_json(&tab),
                                    "root_pane": st.pane_json(&pane, &tab, params.get("cwd"))}})
                            }
                        }
                        "tab.list" => {
                            let wid = wid_of("workspace_id");
                            let tabs: Vec<Value> = st
                                .tabs
                                .lock()
                                .unwrap()
                                .iter()
                                .filter(|t| wid.is_empty() || t.workspace_id == wid)
                                .map(MockState::tab_json)
                                .collect();
                            json!({"id": id, "result": {"type": "tab_list", "tabs": tabs}})
                        }
                        "tab.close" => {
                            let tid = wid_of("tab_id");
                            let mut tabs = st.tabs.lock().unwrap();
                            let before = tabs.len();
                            tabs.retain(|t| t.tab_id != tid);
                            if tabs.len() == before {
                                json!({"id": id, "error": {"code": "tab_not_found", "message": tid}})
                            } else {
                                json!({"id": id, "result": {"type": "ok"}})
                            }
                        }
                        "pane.split" => {
                            let target = wid_of("target_pane_id");
                            let mut tabs = st.tabs.lock().unwrap();
                            match tabs.iter_mut().find(|t| t.panes.iter().any(|p| *p == target)) {
                                None => json!({"id": id, "error": {"code": "pane_not_found", "message": target}}),
                                Some(t) => {
                                    let pane = format!("{}:p{}", t.workspace_id, st.next());
                                    t.panes.push(pane.clone());
                                    json!({"id": id, "result": {"type": "pane_info", "pane":
                                        json!({"pane_id": pane, "workspace_id": t.workspace_id,
                                               "tab_id": t.tab_id, "cwd": params.get("cwd"),
                                               "agent": null, "agent_status": null})}})
                                }
                            }
                        }
                        "pane.close" => {
                            // NB: no tab reaping — see the type comment.
                            let pid = wid_of("pane_id");
                            let mut tabs = st.tabs.lock().unwrap();
                            for t in tabs.iter_mut() {
                                t.panes.retain(|p| *p != pid);
                            }
                            json!({"id": id, "result": {"type": "ok"}})
                        }
                        "pane.get" => {
                            let pid = wid_of("pane_id");
                            match st.find_pane(&pid) {
                                Some(t) => json!({"id": id, "result": {"type": "pane_info",
                                    "pane": st.pane_json(&pid, &t, None)}}),
                                // 只用 `live_pane` 種、沒掛在 tab 上的 pane：照樣回得出畫面列數（#403）。
                                None if st.live.lock().unwrap().get(&pid).is_some_and(|p| p.rows.is_some()) => {
                                    let t = MockTab { tab_id: "w0:t0".into(), workspace_id: "w0".into(), label: String::new(), panes: vec![] };
                                    json!({"id": id, "result": {"type": "pane_info", "pane": st.pane_json(&pid, &t, None)}})
                                }
                                None => json!({"id": id, "error": {"code": "pane_not_found", "message": pid}}),
                            }
                        }
                        "pane.move" => {
                            let pid = wid_of("pane_id");
                            let label = params
                                .get("destination")
                                .and_then(|d| d.get("label"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            match st.find_pane(&pid) {
                                None => json!({"id": id, "error": {"code": "pane_not_found", "message": pid}}),
                                Some(prev) => {
                                    // Old tab is left standing, empty or not.
                                    st.tabs.lock().unwrap().iter_mut().for_each(|t| t.panes.retain(|p| *p != pid));
                                    let n = st.next();
                                    let tab = MockTab {
                                        tab_id: format!("{}:t{n}", prev.workspace_id),
                                        workspace_id: prev.workspace_id.clone(),
                                        label,
                                        panes: vec![pid.clone()],
                                    };
                                    st.tabs.lock().unwrap().push(tab.clone());
                                    json!({"id": id, "result": {"type": "pane_move", "move_result": {
                                        "changed": true,
                                        "previous_pane_id": pid,
                                        "previous_workspace_id": prev.workspace_id,
                                        "previous_tab_id": prev.tab_id,
                                        "created_tab": MockState::tab_json(&tab),
                                        "pane": st.pane_json(&pid, &tab, None)}}})
                                }
                            }
                        }
                        "session.snapshot" => {
                            let tabs = st.tabs.lock().unwrap().clone();
                            let names: Vec<String> = st
                                .agents
                                .lock()
                                .unwrap()
                                .iter()
                                .filter_map(|a| a.get("pane_id").and_then(Value::as_str).map(String::from))
                                .collect();
                            let panes: Vec<Value> = tabs
                                .iter()
                                .flat_map(|t| t.panes.iter().map(|p| (t.workspace_id.clone(), p.clone())))
                                .map(|(ws, p)| json!({"pane_id": p, "workspace_id": ws, "agent": if names.contains(&p) { json!("x") } else { Value::Null }}))
                                .collect();
                            let wss: Vec<Value> = st
                                .workspaces
                                .lock()
                                .unwrap()
                                .iter()
                                .map(|(w, l)| json!({"workspace_id": w, "label": l}))
                                .collect();
                            json!({"id": id, "result": {"type": "session_snapshot", "snapshot":
                                {"workspaces": wss, "panes": panes,
                                 "tabs": tabs.iter().map(MockState::tab_json).collect::<Vec<_>>()}}})
                        }
                        // Same enum as real herdr: an unknown source is a request error, not a read.
                        "pane.read" if !matches!(params.get("source").and_then(Value::as_str),
                            None | Some("visible" | "recent" | "recent_unwrapped" | "detection")) =>
                        {
                            json!({"id": id, "error": {"code": "invalid_request", "message": format!(
                                "invalid request: unknown variant `{}`, expected one of `visible`, `recent`, `recent_unwrapped`, `detection`",
                                params["source"].as_str().unwrap_or_default())}})
                        }
                        "pane.read" if params.get("format").and_then(Value::as_str) == Some("ansi")
                            && st.reject_ansi.load(std::sync::atomic::Ordering::SeqCst) =>
                        {
                            json!({"id": id, "error": {"code": "invalid_params", "message": "unknown field `format`"}})
                        }
                        "pane.read" => {
                            let pid = wid_of("pane_id");
                            let live_read = {
                                let mut live = st.live.lock().unwrap();
                                live.get_mut(&pid).map(|p| {
                                    p.reads += 1;
                                    (p.render(), p.revision.max(1))
                                })
                            };
                            let (text, revision) = live_read.unwrap_or_else(|| {
                                let screens = st.screens.lock().unwrap();
                                let text = screens.get(&pid).cloned().or_else(|| screens.get("*").cloned()).unwrap_or_default();
                                let revisions = st.screen_revisions.lock().unwrap();
                                let revision = revisions.get(&pid).or_else(|| revisions.get("*")).copied().unwrap_or(1);
                                (text, revision)
                            });
                            // A screen set to this marker answers like a broken pane: the caller
                            // must treat a read failure as an error, never as an empty screen.
                            if text == "__READ_ERROR__" {
                                json!({"id": id, "error": {"code": "pane_unavailable", "message": "pane read failed"}})
                            } else {
                                let asked_ansi = params.get("format").and_then(Value::as_str) == Some("ansi");
                                let format = if asked_ansi && !st.ignore_ansi.load(std::sync::atomic::Ordering::SeqCst) { "ansi" } else { "text" };
                                json!({"id": id, "result": {"type": "pane_read", "read": {
                                    "pane_id": pid, "source": params.get("source").cloned().unwrap_or(json!("recent_unwrapped")),
                                    "format": format, "text": text, "revision": revision, "truncated": false}}})
                            }
                        }
                        "pane.process_info" => {
                            let pid = wid_of("pane_id");
                            let os_pid = st.pids.lock().unwrap().get(&pid).copied().unwrap_or(1);
                            let shell_pid = st.shell_pids.lock().unwrap().get(&pid).copied();
                            match st.argvs.lock().unwrap().get(&pid).cloned() {
                                None => json!({"id": id, "result": {"process_info":
                                    {"pane_id": pid, "shell_pid": shell_pid, "foreground_processes": []}}}),
                                Some(argv) => json!({"id": id, "result": {"process_info": {"pane_id": pid, "shell_pid": shell_pid,
                                    "foreground_processes": [{"argv": argv, "argv0": argv.first().cloned(),
                                    "cwd": "/tmp/p", "pid": os_pid}]}}}),
                            }
                        }
                        // Typing into a pane: recorded in `calls`. A live pane reacts like a TUI;
                        // any other pane just answers ok and keeps whatever screen the test set.
                        "pane.layout" => {
                            let pid = wid_of("pane_id");
                            let width = st.live.lock().unwrap().get(&pid).and_then(|p| p.width);
                            match width {
                                Some(w) => json!({"id": id, "result": {"layout": {"panes": [
                                    {"pane_id": pid, "rect": {"x": 0, "y": 0, "width": w, "height": 40}}]}}}),
                                None => json!({"id": id, "error": {"code": "unsupported", "message": "no layout"}}),
                            }
                        }
                        "pane.send_text" => {
                            let pid = wid_of("pane_id");
                            let text = params.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                            if let Some(p) = st.live.lock().unwrap().get_mut(&pid) {
                                let text = match p.send_text_drops_head_over {
                                    Some(cap) if text.len() > cap => {
                                        let mut cut = cap;
                                        while !text.is_char_boundary(cut) {
                                            cut += 1;
                                        }
                                        text[cut..].to_string()
                                    }
                                    _ => text,
                                };
                                if !p.swallow_text {
                                    // 游標一直停在框的最後一列尾巴：下一段的第一行接在後面（貼上被拆成好幾段時）。
                                    for (n, line) in text.split('\n').enumerate() {
                                        match p.composer.last_mut() {
                                            Some(last) if n == 0 => last.push_str(line),
                                            _ => p.composer.push(line.to_string()),
                                        }
                                    }
                                }
                            }
                            json!({"id": id, "result": {"type": "ok"}})
                        }
                        "pane.send_keys" => {
                            let pid = wid_of("pane_id");
                            let keys: Vec<String> = params
                                .get("keys")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter_map(Value::as_str).map(|k| k.to_ascii_lowercase()).collect())
                                .unwrap_or_default();
                            if let Some(p) = st.live.lock().unwrap().get_mut(&pid) {
                                let mut prev = String::new();
                                for k in &keys {
                                    // claude 2.1.275 的 send-now 和弦（issue #103）：`ctrl+x ctrl+s` 跟 Enter
                                    // 一樣把框裡的字送出去，差別在真的 CLI 那邊會順便打斷當下那一回合。
                                    let send_now = k == "ctrl+s" && prev == "ctrl+x";
                                    match k.as_str() {
                                        "enter" if !p.swallow_enter => submit_composer(p),
                                        _ if send_now && !p.swallow_enter => submit_composer(p),
                                        "ctrl+c" => p.composer.clear(),
                                        _ => {}
                                    }
                                    prev.clone_from(k);
                                }
                            }
                            json!({"id": id, "result": {"type": "ok"}})
                        }
                        "agent.send_keys" => {
                            let target = wid_of("target");
                            let ctrl_c = params
                                .get("keys")
                                .and_then(Value::as_array)
                                .map(|keys| keys.iter().any(|k| k.as_str() == Some("ctrl+c")))
                                .unwrap_or(false);
                            if ctrl_c {
                                st.agents
                                    .lock()
                                    .unwrap()
                                    .retain(|a| a.get("name").and_then(Value::as_str) != Some(target.as_str()));
                            }
                            json!({"id": id, "result": {}})
                        }
                        "agent.start" => {
                            let name = wid_of("name");
                            let kind = wid_of("kind");
                            let pane_id = wid_of("pane_id");
                            let args = params
                                .get("args")
                                .and_then(Value::as_array)
                                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                                .unwrap_or_default();
                            let tab = st.find_pane(&pane_id);
                            let (workspace_id, tab_id, cwd) = tab
                                .map(|t| (t.workspace_id, t.tab_id, params.get("cwd").and_then(Value::as_str).unwrap_or("/tmp/p").to_string()))
                                .unwrap_or_else(|| ("ws-1".into(), "tab-1".into(), "/tmp/p".into()));
                            let agent = json!({
                                "name": name,
                                "agent": kind,
                                "agent_status": "idle",
                                "workspace_id": workspace_id,
                                "tab_id": tab_id,
                                "pane_id": pane_id,
                                "cwd": cwd,
                                "interactive_ready": true,
                                "launch_pending": false,
                                "state_change_seq": 1,
                                "revision": 1,
                            });
                            st.argvs.lock().unwrap().insert(pane_id, args);
                            let mut agents = st.agents.lock().unwrap();
                            agents.retain(|a| a.get("name") != Some(&Value::String(name.clone())));
                            agents.push(agent.clone());
                            json!({"id": id, "result": {"agent": agent}})
                        }
                        "agent.rename" => {
                            let target = wid_of("target");
                            let name = params.get("name").and_then(Value::as_str).map(String::from);
                            let mut agents = st.agents.lock().unwrap();
                            let hit = agents.iter_mut().find(|a| {
                                a.get("pane_id").and_then(Value::as_str) == Some(target.as_str())
                                    || a.get("name").and_then(Value::as_str) == Some(target.as_str())
                            });
                            match hit {
                                Some(a) => {
                                    a["name"] = name.map(Value::from).unwrap_or(Value::Null);
                                    json!({"id": id, "result": {"agent": a.clone()}})
                                }
                                None => json!({"id": id, "error": {"code": "agent_not_found", "message": target}}),
                            }
                        }
                        "agent.wait" => {
                            let target = wid_of("target");
                            match st
                                .agents
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|a| a.get("name").and_then(Value::as_str) == Some(target.as_str()))
                                .cloned()
                            {
                                Some(agent) => json!({"id": id, "result": {"agent": agent}}),
                                None => json!({"id": id, "error": {"code": "not_found", "message": target}}),
                            }
                        }
                        "agent.list" => {
                            let agents = if st.hide_agent_list.load(std::sync::atomic::Ordering::SeqCst) {
                                vec![]
                            } else {
                                st.agents.lock().unwrap().clone()
                            };
                            json!({"id": id, "result": {"agents": agents}})
                        }
                        "agent.get" => {
                            // Like herdr: a live agent name or its pane id.
                            let target = wid_of("target");
                            let found = st
                                .agents
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|a| {
                                    a.get("name").and_then(Value::as_str) == Some(target.as_str())
                                        || a.get("pane_id").and_then(Value::as_str) == Some(target.as_str())
                                })
                                .cloned();
                            match found {
                                Some(a) => json!({"id": id, "result": {"agent": a}}),
                                None => json!({"id": id, "error": {"code": "not_found", "message": target}}),
                            }
                        }
                        "events.subscribe" if st.allow_subscribe.load(std::sync::atomic::Ordering::SeqCst) => {
                            let ack = json!({"id": id, "result": {"type": "subscription_started"}});
                            let mut bytes = serde_json::to_vec(&ack).unwrap();
                            bytes.push(b'\n');
                            let _ = w.write_all(&bytes).await;
                            let _ = w.flush().await;
                            // 連線留著、不吐事件：訂閱建好的樣子。
                            std::future::pending::<()>().await;
                            return;
                        }
                        other => json!({"id": id, "error": {"code": "unsupported",
                                        "message": format!("mock herdr does not implement {other}")}}),
                    };
                    if fault == Some(Fault::DropAfter) {
                        return;
                    }
                    let mut bytes = serde_json::to_vec(&out).unwrap();
                    bytes.push(b'\n');
                    let _ = w.write_all(&bytes).await;
                    let _ = w.flush().await;
                });
            }
        });
        MockHerdr { workspaces, tabs, calls, agents, screens, screen_revisions, live, reject_ansi, ignore_ansi, argvs, pids, shell_pids, faults, defer, hide_agent_list, allow_subscribe, pong, handle }
    }

    /// 接下來第一次呼叫 `method` 時照 `fault` 壞一次（排幾次就壞幾次，依序）。呼叫一樣記在 `calls` 裡。
    pub fn fail_next(&self, method: &str, fault: Fault) {
        self.faults.lock().unwrap().push((method.to_string(), fault));
    }

    /// 這個方法先成功 `skip` 次，下一次才套 `fault`。`skip = 1` 是「第 2 次才壞」。
    pub fn fail_after(&self, method: &str, skip: usize, fault: Fault) {
        self.defer.lock().unwrap().push((method.to_string(), skip, fault));
    }

    /// 同 [`MockHerdr::fail_next`]，但拿得進 `race_point` 的 `'static` 閉包：要在某一瞬間之後才壞的時候用（#157）。
    pub fn fail_later(&self) -> impl Fn(&str, Fault) + Send + Sync + 'static {
        let faults = self.faults.clone();
        move |method, fault| faults.lock().unwrap().push((method.to_string(), fault))
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }

    /// 這支方法收到的每一次參數，依序。`first_call` 只看得到第一次；要驗「按了哪些鍵、按了幾次」
    /// 得看全部（issue #103）。
    pub fn calls_to(&self, method: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }

    pub fn first_call(&self, method: &str) -> Option<Value> {
        self.calls.lock().unwrap().iter().find(|(m, _)| m == method).map(|(_, p)| p.clone())
    }

    /// Make this pane behave like a TUI (see [`LivePane`]). Returns nothing; inspect it with
    /// [`MockHerdr::pane`].
    pub fn live_pane(&self, pane_id: &str, pane: LivePane) {
        self.live.lock().unwrap().insert(pane_id.to_string(), pane);
    }

    pub fn pane(&self, pane_id: &str) -> Option<LivePane> {
        self.live.lock().unwrap().get(pane_id).cloned()
    }

    /// Register an agent for `agent.get` / `agent.list`, with or without herdr's session binding.
    pub fn set_agent(&self, name: &str, pane_id: &str, session_bound: bool) {
        let mut agent = serde_json::Map::new();
        agent.insert("name".into(), json!(name));
        agent.insert("agent".into(), json!("claude"));
        agent.insert("agent_status".into(), json!("idle"));
        agent.insert("workspace_id".into(), json!("ws-1"));
        agent.insert("tab_id".into(), json!("tab-1"));
        agent.insert("pane_id".into(), json!(pane_id));
        if session_bound {
            agent.insert("agent_session".into(), json!({"agent": "claude", "kind": "id", "value": "sess-1"}));
        }
        self.agents.lock().unwrap().push(Value::Object(agent));
    }

    pub fn set_screen(&self, pane_id: &str, text: &str) {
        self.screens.lock().unwrap().insert(pane_id.to_string(), text.to_string());
    }

    /// Capture-safe screen replacement for a race point; changing the screen also advances its revision.
    pub fn set_screen_later(&self) -> impl Fn(&str, &str) + Send + Sync + 'static {
        let screens = self.screens.clone();
        let revisions = self.screen_revisions.clone();
        move |pane_id, text| {
            screens.lock().unwrap().insert(pane_id.to_string(), text.to_string());
            let mut revisions = revisions.lock().unwrap();
            let revision = revisions.entry(pane_id.to_string()).or_insert(1);
            *revision = (*revision).saturating_add(1);
        }
    }

    /// Simulate a pane redraw that preserves its visible text but advances the revision.
    pub fn bump_screen_revision_later(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let revisions = self.screen_revisions.clone();
        move |pane_id| {
            let mut revisions = revisions.lock().unwrap();
            let revision = revisions.entry(pane_id.to_string()).or_insert(1);
            *revision = (*revision).saturating_add(1);
        }
    }

    pub fn set_argv(&self, pane_id: &str, argv: &[&str]) {
        self.argvs.lock().unwrap().insert(pane_id.to_string(), argv.iter().map(|s| s.to_string()).collect());
    }

    pub fn set_pid(&self, pane_id: &str, pid: i64) {
        self.pids.lock().unwrap().insert(pane_id.to_string(), pid);
    }

    /// `pane.process_info` 回這個 `shell_pid`（§6.5e：GC 與打字前複查都從它走 `ps` 的行程樹）。
    pub fn set_shell_pid(&self, pane_id: &str, pid: i64) {
        self.shell_pids.lock().unwrap().insert(pane_id.to_string(), pid);
    }

    pub fn tab(&self, tab_id: &str) -> Option<MockTab> {
        self.tabs.lock().unwrap().iter().find(|t| t.tab_id == tab_id).cloned()
    }

    pub fn tabs_in(&self, workspace_id: &str) -> Vec<MockTab> {
        self.tabs.lock().unwrap().iter().filter(|t| t.workspace_id == workspace_id).cloned().collect()
    }
}

pub struct Env {
    pub app: Arc<App>,
    pub project_id: String,
    pub repo: std::path::PathBuf,
    pub dir: std::path::PathBuf,
    pub herdr: MockHerdr,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 測試自己建的暫存資料目錄（`App` 的 `data_dir`）：註冊進來，最後一個 `Arc<App>` 掉了就由 `App` 的 `Drop` 刪掉
/// （`state.rs`）。以前這些 `app()` 輔助函式只建不刪，每跑一輪整樹測試 `/tmp` 多出上千個帶 sqlite 的目錄
/// （2026-10-01 實測 6GB、其中 4GB 是 supervisor 測試），ubuntu-ci 在 17:20 因為 ENOSPC 紅過一輪。
/// 只有註冊過的目錄才會被刪：重開同一個目錄的測試（`restart_app` 之類）不註冊，不受影響。
fn scratch_registry() -> &'static StdMutex<std::collections::HashSet<std::path::PathBuf>> {
    static R: std::sync::OnceLock<StdMutex<std::collections::HashSet<std::path::PathBuf>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// 寫一支測試用的假腳本（`content` 要以 `#!` 開頭）並確定它**已經可以被 exec**（issue #189）：並行的別條測試在別的執行緒
/// `fork` 時會短暫繼承這個檔案的寫入 fd，這段時間 exec 它回 `ETXTBSY`——不管是測試直接 exec，還是被測的程式去 exec。
/// shell 腳本在第一行後面插一行 `AM_TEST_EXEC_PROBE` 的守衛（有設就 `exit 0`），寫完用它 exec 一次、`ETXTBSY` 就重試
/// （[`crate::exec_retry`]，等的是條件不是時間）；exec 成功的那一刻沒有任何行程握著寫入 fd，之後不會再撞。
/// 不是 shell 的腳本（沒有守衛可插）只寫檔加 chmod。
pub fn write_exec(path: impl AsRef<std::path::Path>, content: impl AsRef<str>) {
    use std::os::unix::fs::PermissionsExt as _;
    let (path, content) = (path.as_ref(), content.as_ref());
    let (shebang, rest) = content.split_once('\n').unwrap_or((content, ""));
    assert!(shebang.starts_with("#!"), "script needs a shebang line: {shebang}");
    let is_shell = shebang.split_whitespace().any(|w| matches!(w.rsplit('/').next(), Some("sh" | "bash" | "zsh" | "dash")));
    let body = if is_shell { format!("{shebang}\n[ -z \"${{AM_TEST_EXEC_PROBE:-}}\" ] || exit 0\n{rest}") } else { content.to_string() };
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    if is_shell {
        let out = crate::exec_retry::output(std::process::Command::new(path).env("AM_TEST_EXEC_PROBE", "1")).unwrap();
        assert!(out.status.success(), "{}: {:?}", path.display(), out.status);
    }
}

/// `$TMPDIR/<prefix>-<ulid>`，已建好並註冊。除了 `App` 掉了就刪（見上），測試行程結束時也一定會掃掉（[`remove_at_exit`]）：
/// 只回一個 `PathBuf` 的輔助函式（沒有地方放 guard）靠這一條不留殘骸，測試 panic 也一樣。
pub fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    scratch_registry().lock().unwrap_or_else(|e| e.into_inner()).insert(dir.clone());
    remove_at_exit(&dir);
    dir
}

/// 測試行程結束（`exit`）時刪掉這個檔案或目錄。`libc::atexit` 只掛一次；測試 harness 收尾走 `process::exit`，會跑到。
pub fn remove_at_exit(path: &std::path::Path) {
    static LIST: StdMutex<Vec<std::path::PathBuf>> = StdMutex::new(Vec::new());
    static HOOK: std::sync::Once = std::sync::Once::new();
    extern "C" fn sweep() {
        for p in LIST.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            if std::fs::remove_dir_all(&p).is_err() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    LIST.lock().unwrap_or_else(|e| e.into_inner()).push(path.to_path_buf());
    HOOK.call_once(|| {
        // SAFETY: `sweep` 是沒有參數的 `extern "C"` 函式，整個行程生命週期內都有效。
        unsafe { libc::atexit(sweep) };
    });
}

/// 把測試自己組的暫存路徑（`temp_dir().join(…)`）登記成「行程結束時刪掉」，原樣回傳、**不建立**（有些測試要它先不存在）。
/// 測試 panic 在 `remove_dir_all` 之前、或根本沒寫清理，都不會再留殘骸（issue #763）。
pub fn track(path: std::path::PathBuf) -> std::path::PathBuf {
    remove_at_exit(&path);
    path
}

/// `true`＝這個目錄是 [`scratch_dir`] 建的、現在交還給呼叫端刪。
pub(crate) fn release_scratch(dir: &std::path::Path) -> bool {
    scratch_registry().lock().unwrap_or_else(|e| e.into_inner()).remove(dir)
}

/// The data directory is a **sibling** of the repo, so nothing the daemon writes lands in the checkout.
pub async fn env() -> Env {
    let dir = std::env::temp_dir().join(format!("am-test-{}", db::ulid()));
    let repo = dir.join("repo");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    git::init_repo(&repo);
    let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
    let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
    // 出貨預設是關的（#749 審查）；測試 build 的 hook 本來就是空的，要用的測試換上 stub，所以這裡先把旗標打開，
    // 「預設關」由 config 與 delivery 各自的測試把設定還原成預設再驗。
    cfg.update(|c| {
        c.codex_history.enabled = true;
        Ok(())
    })
    .await
    .unwrap();
    let sock = data.join("herdr.sock");
    let herdr = MockHerdr::start(sock.clone());
    let client = crate::herdr::HerdrClient::new(sock);
    let app = App::new(
        pool,
        client.clone(),
        client,
        cfg,
        data.clone(),
        data.join("agents-managerd"),
        7799,
        "test-token".into(),
        "test".into(),
        false,
    );
    app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    let pid = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?, 'local', ?)")
        .bind(&pid)
        .bind(repo.to_string_lossy().to_string())
        .bind("proj")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    Env { app, project_id: pid, repo, dir, herdr }
}

/// A second `App` on the same database and mock herdr, the way a restarted daemon would open them.
/// In-memory state (timers, pollers) starts empty; everything persisted is still there.
pub async fn restart_app(env: &Env) -> Arc<App> {
    restart_app_lan(env, false).await
}

/// Same as [`restart_app`] with `allow_lan` chosen (the packaged app has it off; dev/LAN daemons turn it on).
pub async fn restart_app_lan(env: &Env, allow_lan: bool) -> Arc<App> {
    let data = env.dir.join("data");
    let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
    let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
    let client = crate::herdr::HerdrClient::new(data.join("herdr.sock"));
    let app = App::new(
        pool,
        client.clone(),
        client,
        cfg,
        data.clone(),
        data.join("agents-managerd"),
        7799,
        "test-token".into(),
        "test".into(),
        allow_lan,
    );
    app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    app
}

/// Straight into the DB, not config.toml.
pub async fn claude_bot(app: &Arc<App>, project_id: &str, name: &str) -> db::Bot {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok','user',?)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(name)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    db::bot(&app.db, &id).await.unwrap().unwrap()
}

/// Looks running to `prompt_grouped` with no live agent; the RPC then fails as
/// `delivery = "unknown"`, which lets a prompt's DB side be tested end to end.
pub async fn fake_run(app: &Arc<App>, bot_id: &str) -> String {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1',?, 'agent', 'test', ?)",
    )
    .bind(&id)
    .bind(bot_id)
    .bind(format!("pane-{bot_id}"))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    id
}

/// 等到條件成立（巨集：條件裡可以 `.await`），回是否在期限內成立。
///
/// 測試裡「等背景工作做完」一律用這個，不要 `sleep(固定時間)` 之後直接斷言、也不要自己寫 `for _ in 0..N` 的短輪詢：
/// 完整測試同時跑上千條，runner 一慢，固定的短等待就偶發紅（#255、#256、quota_refresh）。這裡的期限只是**放棄的上限**
/// （30 秒，條件一成立就立刻回），不是成功的條件。要斷言「沒有發生」的事仍可以固定睡一下再看，那種只會在慢 runner 上少測到，不會翻紅。
macro_rules! eventually {
    ($cond:expr) => {{
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if $cond {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }};
}
pub(crate) use eventually;

/// 讀取故障的注入：`table` 的 SELECT 全部壞掉（`no such table`），到 [`make_table_readable`] 為止。
/// 連線池的每條連線各有自己的 schema 快取：先在**同一條**連線上讀一次 `sqlite_master` 刷新，再 ALTER；
/// 不然剛好抽到沒看過上一次改動的連線，`ALTER` 在編譯階段就 `no such table`（時有時無）。
pub async fn make_table_unreadable(app: &Arc<App>, table: &str) {
    rename_table(app, table, &format!("{table}_unreadable")).await;
}

pub async fn make_table_readable(app: &Arc<App>, table: &str) {
    rename_table(app, &format!("{table}_unreadable"), table).await;
}

async fn rename_table(app: &Arc<App>, from: &str, to: &str) {
    let mut conn = app.db.acquire().await.unwrap();
    sqlx::query("SELECT count(*) FROM sqlite_master").fetch_one(&mut *conn).await.unwrap();
    sqlx::query(&format!("ALTER TABLE {from} RENAME TO {to}")).execute(&mut *conn).await.unwrap();
}

pub mod git {
    //! So the git tests never touch the repo they run in.
    use std::path::PathBuf;
    use std::process::Command;

    pub fn run(dir: &std::path::Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "am-test")
            .env("GIT_AUTHOR_EMAIL", "am-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "am-test")
            .env("GIT_COMMITTER_EMAIL", "am-test@example.invalid")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(o.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    pub fn init_repo(dir: &PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        run(dir, &["init", "--initial-branch=main", "-q"]);
        run(dir, &["config", "user.name", "am-test"]);
        run(dir, &["config", "user.email", "am-test@example.invalid"]);
        run(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "base\n").unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "base"]);
    }
}

#[cfg(test)]
mod write_exec_tests {
    /// 守衛只在探測那一次生效：寫完之後腳本照常跑，參數與 `$0` 都沒變；不是 shell 的腳本原樣寫、不插守衛。
    #[test]
    fn the_probe_guard_is_invisible_to_normal_runs_and_skipped_for_other_interpreters() {
        let dir = super::scratch_dir("am-write-exec");
        let sh = dir.join("echo.sh");
        super::write_exec(&sh, "#!/bin/sh\nprintf '%s|%s' \"$0\" \"$1\"\n");
        let out = std::process::Command::new(&sh).arg("x").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{}|x", sh.display()));
        assert!(std::fs::read_to_string(&sh).unwrap().contains("AM_TEST_EXEC_PROBE"));

        let py = dir.join("noop.py");
        super::write_exec(&py, "#!/usr/bin/env python3\nprint('hi')\n");
        assert!(!std::fs::read_to_string(&py).unwrap().contains("AM_TEST_EXEC_PROBE"), "非 shell 不插守衛");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod scratch_leak_guard {
    /// issue #763：測試裡直接 `std::env::temp_dir().join(…)` 的目錄沒有人保證會刪，整樹跑一輪 `/tmp` 多出上萬個。
    /// 每一處都要過 `testing::track` / `testing::scratch_dir`（行程結束時一定掃掉）。
    #[test]
    fn no_test_builds_an_unregistered_path_under_temp_dir() {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("testing.rs") {
                    for (i, l) in std::fs::read_to_string(&p).unwrap().lines().enumerate() {
                        // `tt::` 是各測試模組 `use crate::testing as tt;` 的慣用別名。
                        if l.contains("temp_dir()") && !["testing::track(", "testing::scratch_dir(", "tt::track(", "tt::scratch_dir("].iter().any(|w| l.contains(w)) {
                            out.push(format!("{}:{}: {}", p.display(), i + 1, l.trim()));
                        }
                    }
                }
            }
        }
        let mut bad = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut bad);
        assert!(bad.is_empty(), "wrap with crate::testing::track(…) or use testing::scratch_dir:\n{}", bad.join("\n"));
    }
}
