//! Shared test fixtures: a mock herdr on a real unix socket, a daemon `App` over a throw-away
//! sqlite file and git repository, and a fake running `runs` row.
use crate::db;
use crate::state::App;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// Mock herdr on a real unix socket, one newline-JSON request per connection. Hook injection and
/// `ensure_kind_installed` probing are deliberately left to a live agent.
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
    /// `pane.process_info` argv, per pane id.
    pub argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
    /// `pane.process_info` pid (default 1); only tests reading the process's account (SPEC §16.6) need it.
    pub pids: Arc<StdMutex<BTreeMap<String, i64>>>,
    /// Every `(method, params)` sent, so a test can assert *how* the daemon asked.
    pub calls: Arc<StdMutex<Vec<(String, Value)>>>,
    /// Panes that behave like a TUI: typing lands in a composer, Enter moves it to the transcript.
    pub live: Arc<StdMutex<BTreeMap<String, LivePane>>>,
    /// What `agent.list` and `agent.get` answer with.
    pub agents: Arc<StdMutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
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
            None => out.push_str("❯\n"),
            Some((first, rest)) => {
                out.push_str(&format!("❯ {first}\n"));
                for r in rest {
                    out.push_str(&format!("  {r}\n"));
                }
            }
        }
        out.push_str("─────────────────────────────────────────────\n");
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
    live: Arc<StdMutex<BTreeMap<String, LivePane>>>,
    argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
    pids: Arc<StdMutex<BTreeMap<String, i64>>>,
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
        json!({"pane_id": pane_id, "workspace_id": tab.workspace_id, "tab_id": tab.tab_id,
               "cwd": cwd.cloned().unwrap_or(Value::Null), "agent": agent, "agent_status": status})
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
            live: Default::default(),
            argvs: Default::default(),
            pids: Default::default(),
            seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        };
        let (workspaces, tabs, calls, agents) =
            (state.workspaces.clone(), state.tabs.clone(), state.calls.clone(), state.agents.clone());
        let (screens, argvs, pids) = (state.screens.clone(), state.argvs.clone(), state.pids.clone());
        let live = state.live.clone();
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
                    let wid_of = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    let out = match method.as_str() {
                        "ping" => json!({"id": id, "result": {"version": "mock", "protocol": 20}}),
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
                                .flat_map(|t| t.panes.iter())
                                .map(|p| json!({"pane_id": p, "agent": if names.contains(p) { json!("x") } else { Value::Null }}))
                                .collect();
                            let wss: Vec<Value> = st
                                .workspaces
                                .lock()
                                .unwrap()
                                .keys()
                                .map(|w| json!({"workspace_id": w}))
                                .collect();
                            json!({"id": id, "result": {"type": "session_snapshot", "snapshot":
                                {"workspaces": wss, "panes": panes,
                                 "tabs": tabs.iter().map(MockState::tab_json).collect::<Vec<_>>()}}})
                        }
                        "pane.read" => {
                            let pid = wid_of("pane_id");
                            let live_text = {
                                let mut live = st.live.lock().unwrap();
                                live.get_mut(&pid).map(|p| {
                                    p.reads += 1;
                                    p.render()
                                })
                            };
                            let text = live_text.unwrap_or_else(|| st.screens.lock().unwrap().get(&pid).cloned().unwrap_or_default());
                            // A screen set to this marker answers like a broken pane: the caller
                            // must treat a read failure as an error, never as an empty screen.
                            if text == "__READ_ERROR__" {
                                json!({"id": id, "error": {"code": "pane_unavailable", "message": "pane read failed"}})
                            } else {
                                json!({"id": id, "result": {"type": "pane_read", "read": {
                                    "pane_id": pid, "source": params.get("source").cloned().unwrap_or(json!("recent_unwrapped")),
                                    "format": "text", "text": text, "revision": 1, "truncated": false}}})
                            }
                        }
                        "pane.process_info" => {
                            let pid = wid_of("pane_id");
                            let os_pid = st.pids.lock().unwrap().get(&pid).copied().unwrap_or(1);
                            match st.argvs.lock().unwrap().get(&pid).cloned() {
                                None => json!({"id": id, "result": {"process_info":
                                    {"pane_id": pid, "foreground_processes": []}}}),
                                Some(argv) => json!({"id": id, "result": {"process_info": {"pane_id": pid,
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
                                if !p.swallow_text {
                                    for line in text.split('\n') {
                                        p.composer.push(line.to_string());
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
                                for k in &keys {
                                    match k.as_str() {
                                        "enter" if !p.swallow_enter => {
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
                                        "ctrl+c" => p.composer.clear(),
                                        _ => {}
                                    }
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
                            json!({"id": id, "result": {"agents": st.agents.lock().unwrap().clone()}})
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
                        other => json!({"id": id, "error": {"code": "unsupported",
                                        "message": format!("mock herdr does not implement {other}")}}),
                    };
                    let mut bytes = serde_json::to_vec(&out).unwrap();
                    bytes.push(b'\n');
                    let _ = w.write_all(&bytes).await;
                    let _ = w.flush().await;
                });
            }
        });
        MockHerdr { workspaces, tabs, calls, agents, screens, live, argvs, pids, handle }
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
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

    pub fn set_argv(&self, pane_id: &str, argv: &[&str]) {
        self.argvs.lock().unwrap().insert(pane_id.to_string(), argv.iter().map(|s| s.to_string()).collect());
    }

    pub fn set_pid(&self, pane_id: &str, pid: i64) {
        self.pids.lock().unwrap().insert(pane_id.to_string(), pid);
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

/// The data directory is a **sibling** of the repo, so nothing the daemon writes lands in the checkout.
pub async fn env() -> Env {
    let dir = std::env::temp_dir().join(format!("am-test-{}", db::ulid()));
    let repo = dir.join("repo");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    git::init_repo(&repo);
    let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
    let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
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
