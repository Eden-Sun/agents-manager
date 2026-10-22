//! 廣域 kill 守門（issue #1884）：PreToolUse hook 在 bot 的 shell 指令跑之前判斷「這條會不會殺一大片行程」。
//!
//! 事故：`pkill -f "next-server" -P 1`——macOS／BSD 的 `pkill` 遇到第一個非選項參數就停止解析選項，`-P 1` 被當成**額外的比對字串**，
//! 結果殺掉命令列含「1」的全部行程（同一台機器上所有 claude／codex／grok 與 Chrome）。
//!
//! 全部是**靜態判斷**：只看指令字串，不跑任何外部指令（不 `pgrep`、不 `ps`），所以不會慢、不會卡住、不會因環境不同而結果不同。
//! 判不出來（解析不了、動態展開的 PID）一律放行——這個 hook 絕不能因為自己的錯擋掉正常指令。門檻與保護名單見 SPEC「廣域 kill 守門」。
//!
//! 只擋**確定廣域**的：
//! * `pkill` 的選項寫在 pattern 之後（BSD 當成 pattern，GNU 當成選項，兩邊語意不同）；
//! * `pkill`／`killall` 的 pattern 命中受保護的行程名（claude／codex／grok／herdr／agents-managerd／agy…）或短到幾乎什麼都命中；
//! * 沒有 pattern 的 `pkill -u …`／`-P 1`、沒有名字的 `killall -u …`；
//! * `kill -1`／`kill -9 -1`／`kill -- -1`／`kill 0`（所有行程／整個行程群組）。
//!
//! `kill <明確 PID>`、`lsof -ti :PORT | xargs kill`、`pkill -f "next-server"` 這類窄的一律放行。

use serde_json::{json, Value};
use std::cell::Cell;

/// `Allow`＝放行；`Deny(原因)`＝擋下，原因直接給 agent 看（含正確做法）。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

/// 不論是否 `-f`，pattern 只要可能命中這些行程名就擋。`node` 在：claude／codex 的 npm 安裝版行程名就是 node。
pub const PROTECTED_NAMES: &[&str] = &["claude", "codex", "grok", "herdr", "agents-managerd", "agy", "antigravity", "node", "sh", "bash", "zsh", "sshd"];

/// `pkill -f` 比對的是整條命令列：用各 agent 實際長相的樣本去試 pattern 會不會命中。
pub const PROTECTED_CMDLINES: &[&str] = &[
    "claude --settings /home/u/.config/agents-manager/bots/01MXXXXXXXXXXXXXXXXXXXXXXX/claude-settings.json --verbose",
    "/home/u/.local/bin/claude --dangerously-skip-permissions",
    "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
    "codex -c notify=[\"agents-managerd\",\"hook\",\"codex\"]",
    "node /usr/lib/node_modules/@openai/codex/bin/codex.js",
    "grok --always-approve",
    "herdr server",
    "agents-managerd serve",
    "agy --dangerously-skip-permissions",
    "antigravity",
];

/// 非 `-x` 的 pattern 少於這個字元數就視為太寬（`pkill -f 1` 命中所有命令列含「1」的行程）。
pub const MIN_PATTERN_CHARS: usize = 3;

const MAX_DEPTH: u32 = 4;

/// pattern（ERE）的長度、群組巢狀、整條指令的比對步數與遞迴深度上限；超過就放行並記 log（見「迷你 ERE」）。
pub const MAX_PATTERN_CHARS: usize = 256;
const MAX_GROUP_DEPTH: u32 = 8;
const MAX_REGEX_STEPS: u32 = 100_000;
const MAX_REGEX_DEPTH: u32 = 256;

/// 命中 `node` 時多講的一段：agent 最常被這個擋到，而且看不出為什麼（`pkill -x node` 看起來很窄）。
pub const NODE_NOTE: &str = "；`node` 也是 claude／codex（npm 安裝版）的行程名，殺掉同名行程會連 agent 自己一起殺掉。要停自己的 node 服務，請改用自己記下的 PID 或 port：`lsof -ti :PORT | xargs kill`，或 `kill <PID>`";

pub const HOW_TO: &str = "只殺自己記下的 PID 或 port：`lsof -ti :PORT | xargs kill`，或 `kill <自己啟動時記下的 PID>`。不要用 pkill／killall 比對寬鬆的字串。";

/// 一次判斷的共用狀態：regex 預算，和「判不出來所以放行」的原因（給 hook 寫 stderr）。
struct Ctx {
    budget: Budget,
    notes: Vec<String>,
}

impl Ctx {
    fn note(&mut self, s: impl Into<String>) {
        if self.notes.len() < 16 {
            self.notes.push(s.into());
        }
    }
}

/// 一整條 shell 指令（可含 `;`、`&&`、`|`、`$(…)`、`bash -c '…'`、heredoc）。第一個要擋的 segment 決定原因。
pub fn check_command(cmd: &str) -> Verdict {
    check_command_logged(cmd).0
}

/// [`check_command`]＋放行時的備註（解析不了、pattern 超過上限…），hook 寫到 stderr。
pub fn check_command_logged(cmd: &str) -> (Verdict, Vec<String>) {
    let mut ctx = Ctx { budget: Budget::new(), notes: Vec::new() };
    let v = check_depth(cmd, 0, &mut ctx);
    (v, ctx.notes)
}

fn check_depth(cmd: &str, depth: u32, ctx: &mut Ctx) -> Verdict {
    if depth > MAX_DEPTH {
        ctx.note("指令巢狀（$()／bash -c／eval）超過上限，放行");
        return Verdict::Allow;
    }
    let script = parse_shell(cmd);
    for n in script.notes {
        ctx.note(n);
    }
    for sub in &script.subs {
        if let Verdict::Deny(r) = check_depth(sub, depth + 1, ctx) {
            return Verdict::Deny(r);
        }
    }
    for simple in &script.cmds {
        if let Verdict::Deny(r) = check_simple(simple, depth, ctx) {
            return Verdict::Deny(r);
        }
    }
    Verdict::Allow
}

fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh")
}

fn check_simple(simple: &Simple, depth: u32, ctx: &mut Ctx) -> Verdict {
    let Some(w) = unwrap_wrappers(&simple.words) else {
        ctx.note("`env -S` 展開次數超過上限，放行");
        return Verdict::Allow;
    };
    let Some(head) = w.first() else { return Verdict::Allow };
    let args = &w[1..];
    let name = basename(head);
    let mut dash_c = false;
    let verdict = match name {
        "kill" => check_kill(args),
        "pkill" => check_pkill(args, ctx),
        "killall" => check_killall(args, ctx),
        n if is_shell(n) => {
            // `bash -n`／`-nv`：只檢查語法、不執行，裡面寫什麼都不會跑。
            let noexec = args.iter().take_while(|a| a.starts_with('-')).any(|a| !a.starts_with("--") && a.len() > 1 && a[1..].contains('n'));
            if noexec {
                return Verdict::Allow;
            }
            // `bash -c '…'`、`bash -lc '…'`：-c 的下一個參數是一段 shell 指令。
            let c = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'));
            match c.and_then(|i| args.get(i + 1)) {
                Some(script) => {
                    dash_c = true;
                    check_depth(script, depth + 1, ctx)
                }
                None => Verdict::Allow,
            }
        }
        "eval" => check_depth(&args.join(" "), depth + 1, ctx),
        _ => Verdict::Allow,
    };
    if let Verdict::Deny(r) = verdict {
        return Verdict::Deny(r);
    }
    // heredoc／here-string：餵給 shell（沒有 -c、沒有 script 檔參數）的就是一份要跑的 script；其他指令的未加引號 heredoc 內文只有 `$(…)`、反引號會被執行。
    for doc in &simple.docs {
        let v = if is_shell(name) && !dash_c && args.iter().all(|a| a.starts_with('-')) {
            check_depth(&doc.body, depth + 1, ctx)
        } else if doc.expands {
            let mut out = Verdict::Allow;
            for sub in doc_subs(&doc.body) {
                if let Verdict::Deny(r) = check_depth(&sub, depth + 1, ctx) {
                    out = Verdict::Deny(r);
                    break;
                }
            }
            out
        } else {
            Verdict::Allow
        };
        if let Verdict::Deny(r) = v {
            return Verdict::Deny(r);
        }
    }
    Verdict::Allow
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        None => false,
    }
}

/// `env -S "字串"`／`env -iS字串`／`env --split-string=字串`：回（那串字, 用掉幾個參數）。
fn env_split_string(rest: &[String]) -> Option<(String, usize)> {
    let a = rest.first()?.as_str();
    if let Some(v) = a.strip_prefix("--split-string=") {
        return Some((v.to_string(), 1));
    }
    if a == "--split-string" {
        return Some((rest.get(1)?.clone(), 2));
    }
    let cluster = a.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    let k = cluster.find('S')?;
    if !cluster[..k].chars().all(|c| "i0v".contains(c)) {
        return None;
    }
    let tail = &cluster[k + 1..];
    if tail.is_empty() { Some((rest.get(1)?.clone(), 2)) } else { Some((tail.to_string(), 1)) }
}

/// `env -S` 一次判斷最多展開幾層（每層要複製剩餘參數、重切字串；`env -S env -S …` 排幾千個會變成二次方）。
const MAX_ENV_S_EXPANSIONS: u32 = 16;

/// 把 `VAR=x`、`sudo -u u`、`env -i`、`env -S "…"`、`nohup`、`timeout 5`、`xargs -n1`… 這些前綴剝掉，回真正的指令字起頭。
/// `None`＝`env -S` 展開超過 [`MAX_ENV_S_EXPANSIONS`]（呼叫端放行並記 log）。
fn unwrap_wrappers(words: &[String]) -> Option<Vec<String>> {
    unwrap_budgeted(words, &mut 0)
}

fn unwrap_budgeted(words: &[String], expansions: &mut u32) -> Option<Vec<String>> {
    let mut w = words;
    loop {
        let Some(head) = w.first() else { return Some(Vec::new()) };
        if is_assignment(head) || matches!(head.as_str(), "!" | "{" | "then" | "do" | "else" | "elif" | "if" | "while" | "until") {
            w = &w[1..];
            continue;
        }
        // 各 wrapper 帶值的選項（其餘選項視為無值旗標）。
        let valued: &[&str] = match basename(head) {
            "sudo" | "doas" => &["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U"],
            "env" => &["-u", "-C", "--unset", "--chdir"],
            "nice" => &["-n"],
            "ionice" => &["-c", "-n", "-p"],
            "timeout" => &["-s", "-k"],
            "xargs" => &["-n", "-I", "-P", "-L", "-d", "-E", "-s", "-a", "-J"],
            "command" | "exec" | "builtin" | "nohup" | "time" | "setsid" | "stdbuf" => &[],
            _ => return Some(w.to_vec()),
        };
        let name = basename(head);
        let mut i = 1;
        while i < w.len() {
            let a = w[i].as_str();
            if a == "--" {
                i += 1;
                break;
            }
            if name == "env" {
                // `-S` 的值是一整串要再切開的指令：切開後接上剩下的參數，從頭再剝。
                if let Some((val, used)) = env_split_string(&w[i..]) {
                    *expansions += 1;
                    if *expansions > MAX_ENV_S_EXPANSIONS {
                        return None;
                    }
                    let mut nw = parse_shell(&val).cmds.into_iter().next().map(|s| s.words).unwrap_or_default();
                    nw.extend(w[i + used..].iter().cloned());
                    return unwrap_budgeted(&nw, expansions);
                }
            }
            if valued.contains(&a) {
                i += 2;
            } else if a.starts_with('-') && a.len() > 1 || (name == "env" && is_assignment(a)) {
                i += 1;
            } else {
                break;
            }
        }
        if name == "timeout" && i < w.len() {
            i += 1; // 時限
        }
        if i >= w.len() {
            return Some(Vec::new());
        }
        w = &w[i..];
    }
}

// ───────────────────────── 切指令 ─────────────────────────

/// heredoc／here-string 的內文。`expands`＝heredoc 的終止字沒加引號（內文裡的 `$(…)`、反引號會被執行）。
struct Doc {
    body: String,
    expands: bool,
}

#[derive(Default)]
struct Simple {
    words: Vec<String>,
    docs: Vec<Doc>,
}

#[derive(Default)]
struct Script {
    cmds: Vec<Simple>,
    /// `$(…)`、反引號裡的指令（不含單引號裡的、註解裡的、加引號 heredoc 內文裡的）。
    subs: Vec<String>,
    notes: Vec<String>,
}

struct PendingDoc {
    delim: String,
    strip_tabs: bool,
    expands: bool,
    owner: usize,
}

/// 掃描時放進字裡的記號：`<<<` 的下一個字是 here-string 內文。
const HERE_STRING: &str = "\u{2}<<<";
/// 動態展開（`$(…)`）在字裡的替身：不是任何已知的 pattern／PID。
const SUB_PLACEHOLDER: &str = "$(..)";

/// `chars[i..]` 是 `$(`：回（括號內的文字, 括號後一格）。引號、巢狀 `$(`、反引號都看；不成對回 `None`。
fn dollar_paren(chars: &[char], i: usize) -> Option<(String, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum M {
        Cmd,
        Sq,
        Dq,
    }
    let mut st = vec![M::Cmd];
    let mut j = i + 2;
    while j < chars.len() {
        let c = chars[j];
        match *st.last()? {
            M::Sq => {
                if c == '\'' {
                    st.pop();
                }
            }
            M::Dq => match c {
                '"' => {
                    st.pop();
                }
                '\\' => j += 1,
                '$' if chars.get(j + 1) == Some(&'(') => {
                    st.push(M::Cmd);
                    j += 1;
                }
                _ => {}
            },
            M::Cmd => match c {
                '\'' => st.push(M::Sq),
                '"' => st.push(M::Dq),
                '\\' => j += 1,
                '$' if chars.get(j + 1) == Some(&'(') => {
                    st.push(M::Cmd);
                    j += 1;
                }
                '(' => st.push(M::Cmd),
                ')' => {
                    st.pop();
                }
                '`' => j = backtick(chars, j)?.1 - 1,
                _ => {}
            },
        }
        j += 1;
        if st.is_empty() {
            return Some((chars[i + 2..j - 1].iter().collect(), j));
        }
    }
    None
}

/// `chars[i]` 是反引號：回（兩個反引號之間的文字, 結尾反引號後一格）。
fn backtick(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '`' => return Some((chars[i + 1..j].iter().collect(), j + 1)),
            _ => {}
        }
        j += 1;
    }
    None
}

/// 未加引號 heredoc 內文裡會被執行的 `$(…)`／反引號（內文沒有引號語意，只有反斜線）。括號不成對就停。
fn doc_subs(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '$' if chars.get(i + 1) == Some(&'(') => match dollar_paren(&chars, i) {
                Some((t, next)) => {
                    out.push(t);
                    i = next;
                    continue;
                }
                None => break,
            },
            '`' => match backtick(&chars, i) {
                Some((t, next)) => {
                    out.push(t);
                    i = next;
                    continue;
                }
                None => break,
            },
            _ => {}
        }
        i += 1;
    }
    out
}

/// `<<` 後面的終止字：回（終止字, `<<-`, 有加引號, 終止字後一格）。
fn heredoc_delim(chars: &[char], mut j: usize) -> Option<(String, bool, bool, usize)> {
    let strip = chars.get(j) == Some(&'-');
    if strip {
        j += 1;
    }
    while matches!(chars.get(j), Some(' ' | '\t')) {
        j += 1;
    }
    let (mut d, mut quoted) = (String::new(), false);
    while let Some(&c) = chars.get(j) {
        match c {
            '\'' | '"' => {
                quoted = true;
                j += 1;
                while j < chars.len() && chars[j] != c {
                    d.push(chars[j]);
                    j += 1;
                }
                j += 1;
            }
            '\\' => {
                quoted = true;
                j += 1;
                if let Some(&n) = chars.get(j) {
                    d.push(n);
                    j += 1;
                }
            }
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
            _ => {
                d.push(c);
                j += 1;
            }
        }
    }
    if d.is_empty() { None } else { Some((d, strip, quoted, j.min(chars.len()))) }
}

/// 從 `pos`（換行後一格）讀掉等著的 heredoc 內文，掛到各自的指令上；回讀完的位置。沒有終止行就讀到結尾（bash 也是）。
fn consume_docs(chars: &[char], mut pos: usize, pending: &mut Vec<PendingDoc>, cmds: &mut [Simple]) -> usize {
    for p in pending.drain(..) {
        let mut body = String::new();
        while pos < chars.len() {
            let end = chars[pos..].iter().position(|&c| c == '\n').map_or(chars.len(), |n| pos + n);
            let line: String = chars[pos..end].iter().collect();
            pos = (end + 1).min(chars.len());
            let cmp = if p.strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            if cmp == p.delim {
                break;
            }
            body.push_str(&line);
            body.push('\n');
        }
        if let Some(c) = cmds.get_mut(p.owner) {
            c.docs.push(Doc { body, expands: p.expands });
        }
    }
    pos
}

fn flush_word(cur: &mut String, has: &mut bool, words: &mut Vec<String>) {
    if *has {
        words.push(std::mem::take(cur));
        *has = false;
    }
}

/// 依 `; & | \n ( )` 與引號切成多條簡單指令，每條是去引號後的字；`$(…)`、反引號另外收進 `subs`（字裡留 [`SUB_PLACEHOLDER`]）。
/// `# 註解` 到行尾略過；heredoc 內文不當指令（餵給 shell 的另當 script 看）。
/// 引號或 `$(` 沒閉合＝shell 也會語法錯誤、整串不會跑：丟掉那一行起的東西、記一筆、其餘照看（呼叫端因此放行）。
fn parse_shell(cmd: &str) -> Script {
    let chars: Vec<char> = cmd.chars().collect();
    let mut s = Script::default();
    let mut raw: Vec<Simple> = Vec::new();
    let mut subs: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut pending: Vec<PendingDoc> = Vec::new();
    let (mut line_cmds, mut line_subs) = (0usize, 0usize);
    let (mut i, mut single, mut double) = (0, false, false);
    let mut failed: Option<&str> = None;
    while i < chars.len() {
        let c = chars[i];
        if !single && c == '$' && chars.get(i + 1) == Some(&'(') {
            let Some((text, next)) = dollar_paren(&chars, i) else {
                failed = Some("`$(` 的括號或引號不成對");
                break;
            };
            subs.push(text);
            cur.push_str(SUB_PLACEHOLDER);
            has = true;
            i = next;
            continue;
        }
        if !single && c == '`' {
            let Some((text, next)) = backtick(&chars, i) else {
                failed = Some("反引號不成對");
                break;
            };
            subs.push(text);
            cur.push_str(SUB_PLACEHOLDER);
            has = true;
            i = next;
            continue;
        }
        if single {
            if c == '\'' {
                single = false;
            } else {
                cur.push(c);
            }
        } else if double {
            match c {
                '"' => double = false,
                '\\' if i + 1 < chars.len() => {
                    i += 1;
                    cur.push(chars[i]);
                }
                _ => cur.push(c),
            }
        } else {
            match c {
                '\'' => {
                    single = true;
                    has = true;
                }
                '"' => {
                    double = true;
                    has = true;
                }
                '\\' if i + 1 < chars.len() => {
                    i += 1;
                    cur.push(chars[i]);
                    has = true;
                }
                '#' if !has => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    continue;
                }
                ' ' | '\t' | '\r' => flush_word(&mut cur, &mut has, &mut words),
                '<' if chars.get(i + 1) == Some(&'<') => {
                    flush_word(&mut cur, &mut has, &mut words);
                    if chars.get(i + 2) == Some(&'<') {
                        words.push(HERE_STRING.to_string());
                        i += 3;
                        continue;
                    }
                    if let Some((delim, strip_tabs, quoted, next)) = heredoc_delim(&chars, i + 2) {
                        pending.push(PendingDoc { delim, strip_tabs, expands: !quoted, owner: raw.len() });
                        i = next;
                        continue;
                    }
                    cur.push(c);
                    has = true;
                }
                // `2>&1`、`&>file`、`>&2` 的 `&` 是重導向的一部分，不是背景符號。
                '&' if cur.ends_with(['>', '<']) || chars.get(i + 1) == Some(&'>') => {
                    cur.push(c);
                    has = true;
                }
                ';' | '&' | '|' | '\n' | '(' | ')' => {
                    flush_word(&mut cur, &mut has, &mut words);
                    if !words.is_empty() {
                        raw.push(Simple { words: std::mem::take(&mut words), docs: Vec::new() });
                    }
                    if c == '\n' {
                        i = if pending.is_empty() { i + 1 } else { consume_docs(&chars, i + 1, &mut pending, &mut raw) };
                        (line_cmds, line_subs) = (raw.len(), subs.len());
                        continue;
                    }
                }
                _ => {
                    cur.push(c);
                    has = true;
                }
            }
        }
        i += 1;
    }
    if failed.is_none() && (single || double) {
        failed = Some("引號沒有閉合");
    }
    if let Some(why) = failed {
        raw.truncate(line_cmds);
        subs.truncate(line_subs);
        words.clear();
        s.notes.push(format!("指令解析失敗（{why}），那一行起不判斷、放行"));
    } else {
        flush_word(&mut cur, &mut has, &mut words);
        if !words.is_empty() {
            raw.push(Simple { words, docs: Vec::new() });
        }
    }
    for simple in raw {
        let (words, mut docs) = strip_redirects(simple.words);
        docs.splice(0..0, simple.docs);
        if !words.is_empty() {
            s.cmds.push(Simple { words, docs });
        }
    }
    s.subs = subs;
    s
}

/// 重導向（`2>/dev/null`、`> out`、`2>&1`、`<in`）不是參數：連同它的目標檔一起拿掉。`<<<` 的下一個字收成 here-string 內文。
fn strip_redirects(words: Vec<String>) -> (Vec<String>, Vec<Doc>) {
    let mut out = Vec::with_capacity(words.len());
    let mut docs = Vec::new();
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
        if w == HERE_STRING {
            if let Some(body) = it.next() {
                docs.push(Doc { body, expands: false });
            }
            continue;
        }
        let op = w.trim_start_matches(|c: char| c.is_ascii_digit());
        if op.starts_with(['>', '<']) || op.starts_with("&>") {
            // 只有運算子、沒有目標（`> file`）：下一個字才是目標。
            if matches!(op, ">" | ">>" | "<" | "&>" | "&>>" | ">|") {
                it.next();
            }
            continue;
        }
        out.push(w);
    }
    (out, docs)
}

// ───────────────────────── kill ─────────────────────────

fn is_signal_spec(a: &str) -> bool {
    let Some(rest) = a.strip_prefix('-') else { return false };
    let rest = rest.strip_prefix("SIG").unwrap_or(rest);
    !rest.is_empty() && (rest.chars().all(|c| c.is_ascii_digit()) || rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

fn check_kill(args: &[String]) -> Verdict {
    let mut targets: Vec<&str> = Vec::new();
    let (mut signal, mut dashdash, mut i) = (false, false, 0);
    while i < args.len() {
        let a = args[i].as_str();
        if dashdash {
            targets.push(a);
        } else if a == "--" {
            dashdash = true;
        } else if a == "-l" || a == "-L" || a == "-t" {
            return Verdict::Allow; // 列出訊號名，不殺任何東西
        } else if a == "-s" || a == "-n" {
            signal = true;
            i += 1;
        } else if !signal && is_signal_spec(a) {
            signal = true;
        } else {
            targets.push(a);
        }
        i += 1;
    }
    // `kill -1`：只有 `-1` 一個參數（沒有目標），照清單一起擋；`kill -1 1234` 不是。
    if args.len() == 1 && args[0] == "-1" {
        return Verdict::Deny(format!("擋下廣域 kill：`kill -1` 是 pid -1（這個使用者的所有行程）的寫法。{HOW_TO}"));
    }
    // 訊號之後的 `-1`／`0`：所有行程／自己的整個行程群組。`kill -1 1234`（對 1234 送 SIGHUP）不是。
    if let Some(t) = targets.iter().find(|t| **t == "-1" || **t == "0") {
        let what = if *t == "0" { "0（呼叫端整個行程群組，包含 agent 自己和上層的 shell）" } else { "-1（這個使用者的所有行程）" };
        return Verdict::Deny(format!("擋下廣域 kill：目標是 pid {what}。{HOW_TO}"));
    }
    Verdict::Allow
}

// ───────────────────────── pkill／killall ─────────────────────────

#[doc(hidden)]
#[derive(Default, Debug)]
pub struct Parsed {
    /// 短旗標字元（不含帶值的）與長旗標名。
    pub flags: Vec<String>,
    /// 帶值的選項（短的存字元、長的存名字）。
    pub values: Vec<(String, String)>,
    pub patterns: Vec<String>,
}

impl Parsed {
    pub fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
    pub fn value(&self, names: &[&str]) -> Option<&str> {
        self.values.iter().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| v.as_str())
    }
}

pub const SIGNAL_NAMES: &[&str] = &["HUP", "INT", "QUIT", "KILL", "TERM", "USR1", "USR2", "STOP", "CONT", "ALRM", "TSTP", "PIPE", "ABRT"];

fn pkill_valued_short(c: char) -> bool {
    "PUugGstTFJjMN".contains(c)
}

/// `bsd=true`：第一個非選項參數起全是 pattern（BSD getopt）；`false`：GNU 式，選項可以夾在 pattern 之間。
#[doc(hidden)]
pub fn parse_pkill(args: &[String], bsd: bool) -> Parsed {
    parse_args(args, bsd, pkill_valued_short, &["parent", "euid", "uid", "pgroup", "group", "session", "terminal", "signal", "pidfile", "runstates", "ns", "nslist", "older", "newer"])
}

fn parse_args(args: &[String], bsd: bool, valued_short: fn(char) -> bool, valued_long: &[&str]) -> Parsed {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            p.patterns.extend(args[i + 1..].iter().cloned());
            break;
        }
        if !(a.starts_with('-') && a.len() > 1) {
            if bsd {
                p.patterns.extend(args[i..].iter().cloned());
                break;
            }
            p.patterns.push(a.to_string());
        } else if a.strip_prefix('-').is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()) || SIGNAL_NAMES.contains(&s.strip_prefix("SIG").unwrap_or(s))) {
            // 訊號：`-9`、`-TERM`、`-SIGKILL`
        } else if let Some(long) = a.strip_prefix("--") {
            match long.split_once('=') {
                Some((k, v)) => p.values.push((k.to_string(), v.to_string())),
                None if valued_long.contains(&long) && i + 1 < args.len() => {
                    p.values.push((long.to_string(), args[i + 1].clone()));
                    i += 1;
                }
                None => p.flags.push(long.to_string()),
            }
        } else {
            let cluster: Vec<char> = a[1..].chars().collect();
            for (k, &ch) in cluster.iter().enumerate() {
                if valued_short(ch) {
                    let rest: String = cluster[k + 1..].iter().collect();
                    if !rest.is_empty() {
                        p.values.push((ch.to_string(), rest));
                    } else if i + 1 < args.len() {
                        p.values.push((ch.to_string(), args[i + 1].clone()));
                        i += 1;
                    }
                    break;
                }
                p.flags.push(ch.to_string());
            }
        }
        i += 1;
    }
    p
}

/// BSD 的 getopt 在第一個非選項參數就停：它之後、`--` 之前所有以 `-` 開頭的字都會被當成 pattern。
fn misordered_options(args: &[String]) -> Vec<String> {
    let bsd = parse_pkill(args, true);
    let gnu = parse_pkill(args, false);
    // GNU 當選項、BSD 當 pattern 的那幾個字：BSD 的 patterns 裡以 `-` 開頭、而且不在 `--` 之後的。
    let after_dd = args.iter().position(|a| a == "--").map_or(&[][..], |i| &args[i + 1..]);
    if bsd.patterns.len() == gnu.patterns.len() {
        return Vec::new();
    }
    bsd.patterns.iter().filter(|p| p.starts_with('-') && p.len() > 1 && !after_dd.contains(p)).cloned().collect()
}

fn check_pkill(args: &[String], ctx: &mut Ctx) -> Verdict {
    let bad = misordered_options(args);
    if !bad.is_empty() {
        return Verdict::Deny(format!(
            "擋下 pkill：選項 {} 寫在比對字串之後。macOS／BSD 的 pkill 遇到第一個非選項參數就停止解析選項，它們會被當成**額外的比對字串**\
             （`pkill -f x -P 1` 會變成殺掉所有命令列含「1」的行程，2026-10-21 #1884 因此殺光整台機器的 agent）；Linux 的 pkill 卻當選項，兩邊行為不同。\
             選項一律寫在 pattern 之前（`pkill -P 1 -f x`）。{HOW_TO}",
            bad.join(" ")
        ));
    }
    let p = parse_pkill(args, true);
    let (full, exact, icase) = (p.has("f") || p.has("full"), p.has("x") || p.has("exact"), p.has("i") || p.has("ignore-case"));
    if p.patterns.is_empty() {
        let by_owner = p.value(&["u", "U", "G", "g", "s", "t", "euid", "uid", "group", "pgroup", "session", "terminal"]).is_some();
        let by_init = p.value(&["P", "parent"]).is_some_and(|v| v == "0" || v == "1");
        if by_owner || by_init {
            return Verdict::Deny(format!("擋下 pkill：沒有比對字串，只用 -u／-g／-s／-t／-P 1 這類條件，會殺掉一整批行程（含其他 agent）。{HOW_TO}"));
        }
        return Verdict::Allow;
    }
    for pat in &p.patterns {
        if let Some(why) = broad_pattern(pat, full, exact, icase, ctx) {
            return Verdict::Deny(format!("擋下 pkill：比對字串 `{pat}` {why}。{HOW_TO}"));
        }
    }
    Verdict::Allow
}

fn check_killall(args: &[String], ctx: &mut Ctx) -> Verdict {
    // killall 的 `-s`：Linux 是帶訊號的選項、BSD 是「只顯示」旗標；後面接得像訊號才當帶值。
    let mut fixed: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-s" && args.get(i + 1).is_some_and(|n| is_signal_spec(&format!("-{n}"))) {
            i += 2;
            continue;
        }
        fixed.push(args[i].clone());
        i += 1;
    }
    let p = parse_args(&fixed, false, |c| "uoyZtc".contains(c), &["user", "older-than", "younger-than", "context"]);
    // `-r`（Linux／psmisc）與 `-m`（BSD）都是「把名稱當 regex」。
    let (regex, icase) = (p.has("r") || p.has("m") || p.has("regexp"), p.has("I") || p.has("ignore-case"));
    if p.patterns.is_empty() {
        if p.value(&["u", "user", "t", "c"]).is_some() {
            return Verdict::Deny(format!("擋下 killall：沒有行程名稱、只用 -u／-t／-c 條件，會殺掉那個使用者／終端的所有行程（含其他 agent）。{HOW_TO}"));
        }
        return Verdict::Allow;
    }
    for name in &p.patterns {
        let hit = if regex {
            broad_pattern(name, false, false, icase, ctx)
        } else {
            PROTECTED_NAMES.iter().find(|n| eq(n, name, icase)).map(|n| format!("就是受保護的行程名 `{n}`（killall 會殺掉同名的全部行程，包含其他 agent 與使用者的 session）{}", if *n == "node" { NODE_NOTE } else { "" }))
        };
        if let Some(why) = hit {
            return Verdict::Deny(format!("擋下 killall：`{name}` {why}。{HOW_TO}"));
        }
    }
    Verdict::Allow
}

fn eq(a: &str, b: &str, icase: bool) -> bool {
    if icase { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// pattern 命中受保護行程、或太短，回原因。
fn broad_pattern(pat: &str, full: bool, exact: bool, icase: bool, ctx: &mut Ctx) -> Option<String> {
    if !exact && pat.chars().count() < MIN_PATTERN_CHARS {
        return Some(format!("只有 {} 個字元，會命中大量不相干的行程（例如命令列含該字串的所有 agent）", pat.chars().count()));
    }
    let subjects: &[&str] = if full { PROTECTED_CMDLINES } else { PROTECTED_NAMES };
    let mut hit: Option<&str> = None;
    for s in subjects {
        match regex_match_in(&ctx.budget, pat, s, icase, exact) {
            Ok(true) => {
                hit = Some(*s);
                break;
            }
            Ok(false) => {}
            Err(why) => {
                // 判不出來不是「不命中」：放行，並留下原因。
                let shown: String = pat.chars().take(40).collect();
                ctx.note(format!("pattern `{shown}` {why}，放行"));
                return None;
            }
        }
    }
    let hit = hit?;
    let name = if full { hit.split_whitespace().next().map(basename).unwrap_or(hit) } else { hit };
    let note = if name == "node" { NODE_NOTE } else { "" };
    Some(format!("會命中受保護的 agent 行程（`{name}`）{note}"))
}

// ───────────────────────── 迷你 ERE ─────────────────────────
// pkill 的 pattern 是 ERE。這裡只需要「這個 pattern 會不會命中這個樣本」：支援 `. * + ? | ( ) [ ] ^ $ \x`，
// 其他（`{n,m}`、反向參照…）一律當字面字元；pattern 語法壞掉就退回「樣本含有這串字」的子字串判斷。
// pattern 是 agent 隨手寫的字串，不能信任：長度、群組巢狀、整條指令的比對步數與遞迴深度都有上限，
// 超過就回 `Err`（呼叫端放行並記 log），不讓 hook 卡住或把堆疊吃光。

#[derive(Debug, Clone)]
enum Atom {
    Lit(char),
    Any,
    Class(Vec<(char, char)>, bool),
    Group(Vec<Vec<Item>>),
    Bol,
    Eol,
}

#[derive(Debug, Clone)]
struct Item {
    atom: Atom,
    min: usize,
    max: usize,
}

/// 一次 `check_command` 共用的比對預算（含所有 pattern × 所有樣本）。
struct Budget {
    steps: Cell<u32>,
    depth: Cell<u32>,
    hit: Cell<Option<&'static str>>,
}

impl Budget {
    fn new() -> Self {
        Budget { steps: Cell::new(0), depth: Cell::new(0), hit: Cell::new(None) }
    }
    /// 進一層比對；超限時記下原因、回 false（呼叫端就地返回，不必 `leave`）。
    fn enter(&self) -> bool {
        if self.hit.get().is_some() {
            return false;
        }
        let steps = self.steps.get() + 1;
        self.steps.set(steps);
        if steps > MAX_REGEX_STEPS {
            self.hit.set(Some("比對運算量超過上限"));
            return false;
        }
        if self.depth.get() >= MAX_REGEX_DEPTH {
            self.hit.set(Some("比對遞迴深度超過上限"));
            return false;
        }
        self.depth.set(self.depth.get() + 1);
        true
    }
    fn leave(&self) {
        self.depth.set(self.depth.get() - 1);
    }
}

enum ParseErr {
    /// 語法壞掉：退回子字串判斷。
    Broken,
    /// 超過資源上限：放行。
    Limit,
}

fn parse_alt(c: &[char], i: &mut usize, nested: u32) -> Result<Vec<Vec<Item>>, ParseErr> {
    let mut alts = vec![Vec::new()];
    while *i < c.len() {
        let ch = c[*i];
        *i += 1;
        let atom = match ch {
            '|' => {
                alts.push(Vec::new());
                continue;
            }
            ')' if nested > 0 => return Ok(alts),
            '(' => {
                if nested >= MAX_GROUP_DEPTH {
                    return Err(ParseErr::Limit);
                }
                Atom::Group(parse_alt(c, i, nested + 1)?)
            }
            '.' => Atom::Any,
            '^' => Atom::Bol,
            '$' => Atom::Eol,
            '\\' => {
                let e = *c.get(*i).ok_or(ParseErr::Broken)?;
                *i += 1;
                match e {
                    'b' | 'B' | '<' | '>' => continue, // 字界：不影響「會不會命中」的保守判斷
                    'd' => Atom::Class(vec![('0', '9')], false),
                    'w' => Atom::Class(vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')], false),
                    's' => Atom::Class(vec![(' ', ' '), ('\t', '\t')], false),
                    other => Atom::Lit(other),
                }
            }
            '[' => {
                let neg = c.get(*i) == Some(&'^');
                if neg {
                    *i += 1;
                }
                let mut ranges = Vec::new();
                let mut first = true;
                loop {
                    let x = *c.get(*i).ok_or(ParseErr::Broken)?;
                    *i += 1;
                    if x == ']' && !first {
                        break;
                    }
                    first = false;
                    if c.get(*i) == Some(&'-') && c.get(*i + 1).is_some_and(|&y| y != ']') {
                        ranges.push((x, c[*i + 1]));
                        *i += 2;
                    } else {
                        ranges.push((x, x));
                    }
                }
                Atom::Class(ranges, neg)
            }
            '*' | '+' | '?' => return Err(ParseErr::Broken), // 前面沒有東西可重複
            other => Atom::Lit(other),
        };
        let (min, max) = match c.get(*i) {
            Some('*') => (0, usize::MAX),
            Some('+') => (1, usize::MAX),
            Some('?') => (0, 1),
            _ => (1, 1),
        };
        if (min, max) != (1, 1) {
            *i += 1;
        }
        alts.last_mut().ok_or(ParseErr::Broken)?.push(Item { atom, min, max });
    }
    if nested > 0 { Err(ParseErr::Broken) } else { Ok(alts) }
}

fn atom_one(a: &Atom, ch: char, icase: bool) -> bool {
    let f = |x: char| if icase { x.to_ascii_lowercase() } else { x };
    match a {
        Atom::Lit(l) => f(*l) == f(ch),
        Atom::Any => true,
        Atom::Class(r, neg) => r.iter().any(|&(lo, hi)| (f(lo)..=f(hi)).contains(&f(ch))) != *neg,
        _ => false,
    }
}

fn match_seq(seq: &[Item], text: &[char], pos: usize, icase: bool, b: &Budget, k: &mut dyn FnMut(usize) -> bool) -> bool {
    if !b.enter() {
        return false;
    }
    let r = match_seq_inner(seq, text, pos, icase, b, k);
    b.leave();
    r
}

fn match_seq_inner(seq: &[Item], text: &[char], pos: usize, icase: bool, b: &Budget, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let Some((item, rest)) = seq.split_first() else { return k(pos) };
    match &item.atom {
        Atom::Bol => pos == 0 && match_seq(rest, text, pos, icase, b, k),
        Atom::Eol => pos == text.len() && match_seq(rest, text, pos, icase, b, k),
        Atom::Group(alts) => {
            #[allow(clippy::too_many_arguments)]
            fn rep(item: &Item, alts: &[Vec<Item>], rest: &[Item], text: &[char], pos: usize, n: usize, icase: bool, b: &Budget, k: &mut dyn FnMut(usize) -> bool) -> bool {
                if n < item.max {
                    for alt in alts {
                        let mut next = |p: usize| (p != pos || n < item.min) && rep(item, alts, rest, text, p, n + 1, icase, b, k);
                        if match_seq(alt, text, pos, icase, b, &mut next) {
                            return true;
                        }
                        if b.hit.get().is_some() {
                            return false;
                        }
                    }
                }
                n >= item.min && match_seq(rest, text, pos, icase, b, k)
            }
            rep(item, alts, rest, text, pos, 0, icase, b, k)
        }
        atom => {
            let mut n = 0;
            while n < item.max && pos + n < text.len() && atom_one(atom, text[pos + n], icase) {
                n += 1;
            }
            loop {
                if n >= item.min && match_seq(rest, text, pos + n, icase, b, k) {
                    return true;
                }
                if n == 0 || n <= item.min || b.hit.get().is_some() {
                    return false;
                }
                n -= 1;
            }
        }
    }
}

/// `exact`＝`pkill -x`（整串都要符合）；否則是子字串搜尋（`pkill`／`pgrep` 的預設）。
/// `Err`＝超過資源上限（原因給 log）：不是「不命中」，呼叫端要放行。
fn regex_match_in(b: &Budget, pat: &str, text: &str, icase: bool, exact: bool) -> Result<bool, &'static str> {
    let chars: Vec<char> = pat.chars().collect();
    if chars.len() > MAX_PATTERN_CHARS {
        return Err("pattern 超過長度上限");
    }
    let mut i = 0;
    let alts = match parse_alt(&chars, &mut i, 0) {
        Ok(a) => a,
        Err(ParseErr::Limit) => return Err("pattern 群組巢狀超過上限"),
        Err(ParseErr::Broken) => {
            let (p, t) = if icase { (pat.to_ascii_lowercase(), text.to_ascii_lowercase()) } else { (pat.to_string(), text.to_string()) };
            return Ok(if exact { p == t } else { t.contains(&p) });
        }
    };
    let t: Vec<char> = text.chars().collect();
    let starts = if exact { 0..=0 } else { 0..=t.len() };
    for s in starts {
        for alt in &alts {
            let mut k = |end: usize| !exact || end == t.len();
            if match_seq(alt, &t, s, icase, b, &mut k) {
                return Ok(true);
            }
            if let Some(why) = b.hit.get() {
                return Err(why);
            }
        }
    }
    Ok(false)
}

/// 獨立一次比對（測試用）：超限當作「不命中」。
#[doc(hidden)]
pub fn regex_match(pat: &str, text: &str, icase: bool, exact: bool) -> bool {
    regex_match_in(&Budget::new(), pat, text, icase, exact).unwrap_or(false)
}

/// 同 [`regex_match`]，但把「超限」跟「不命中」分開（測試用）。
#[doc(hidden)]
pub fn regex_match_limited(pat: &str, text: &str, icase: bool, exact: bool) -> Result<bool, &'static str> {
    regex_match_in(&Budget::new(), pat, text, icase, exact)
}

// ───────────────────────── hook 接線 ─────────────────────────

/// 各 CLI 的 PreToolUse payload（claude／codex 的 `tool_input.command`、grok 的 camelCase 副本、agy 的 `CommandLine`…）裡的 shell 指令。
/// 欄位名每家不同，所以在 tool 輸入物件裡找 `command`／`commandLine`／`cmd` 這幾個字串欄位（最多往下 4 層）。沒有就是 `None`（不是 shell 工具）。
pub fn command_from_payload(payload: &Value) -> Option<String> {
    fn find(v: &Value, depth: u32) -> Option<String> {
        if depth > 4 {
            return None;
        }
        let o = v.as_object()?;
        for (k, val) in o {
            if matches!(k.to_ascii_lowercase().as_str(), "command" | "commandline" | "cmd") {
                if let Some(s) = val.as_str() {
                    return Some(s.to_string());
                }
            }
        }
        o.values().find_map(|c| find(c, depth + 1))
    }
    let input = ["tool_input", "toolInput", "input", "args", "toolCall", "tool_call"].iter().find_map(|k| payload.get(*k))?;
    find(input, 0)
}

fn norm_event(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

fn event_is_pre(event: &str) -> bool {
    norm_event(event) == "pretooluse"
}

fn is_pre_tool_use(event: &str, payload: &Value) -> bool {
    let from_payload = ["hook_event_name", "hookEventName", "event"].iter().find_map(|k| payload.get(*k).and_then(Value::as_str));
    event_is_pre(event) || from_payload.is_some_and(event_is_pre)
}

/// [`guard`] 的結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Guard {
    /// 不是 PreToolUse（或根本讀不懂）：呼叫端照原本的事件流程走。
    NotPreToolUse,
    /// 是 PreToolUse，放行（不是 shell 指令、解析不了、指令是窄的）：stdout 保持空。
    Pass,
    /// 是 PreToolUse，要擋：stdout 印這段決策 JSON（claude／codex／grok 的 `hookSpecificOutput` 形狀；agy 另加 `decision`／`reason`）。
    Deny(String),
}

/// hook 的入口：判斷結果照 [`Guard`]；「判不出來所以放行」的原因各寫一行到 stderr（`hook` 子命令的 stdout 只留給決策 JSON）。
pub fn guard(provider: &str, event: &str, stdin_text: &str) -> Guard {
    guard_logged(provider, event, stdin_text, &mut |m| {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "agents-managerd hook: kill_guard: {m}");
    })
}

/// [`guard`]，log 交給呼叫端（測試收集用）。內部 panic 也接住、放行並記一行：守門自己的錯不能擋掉正常指令。
pub fn guard_logged(provider: &str, event: &str, stdin_text: &str, log: &mut dyn FnMut(&str)) -> Guard {
    let Ok(payload) = serde_json::from_str::<Value>(stdin_text) else {
        // 事件名在參數裡（agy）或 payload 文字裡看得出是 PreToolUse 才記：其他事件的壞 payload 是 daemon 事件流程的事。
        if event_is_pre(event) {
            log("PreToolUse payload 不是合法 JSON，放行");
            return Guard::Pass;
        }
        if norm_event(stdin_text).contains("pretooluse") {
            log("payload 不是合法 JSON（看起來是 PreToolUse，可能被截斷），放行");
        }
        return Guard::NotPreToolUse;
    };
    if !is_pre_tool_use(event, &payload) {
        return Guard::NotPreToolUse;
    }
    let Some(command) = command_from_payload(&payload) else { return Guard::Pass };
    let checked = std::panic::catch_unwind(|| check_command_logged(&command));
    let Ok((verdict, notes)) = checked else {
        log("判斷時內部錯誤，放行");
        return Guard::Pass;
    };
    for n in &notes {
        log(n);
    }
    let Verdict::Deny(reason) = verdict else { return Guard::Pass };
    let mut out = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    if provider == "agy" {
        out["decision"] = json!("deny");
        out["reason"] = json!(reason);
    }
    Guard::Deny(out.to_string())
}
