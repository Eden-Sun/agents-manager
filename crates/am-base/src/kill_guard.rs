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

/// pattern（ERE）可判斷語法的長度與群組巢狀上限（見「迷你 ERE」）；超過就放行並記 log。
pub const MAX_PATTERN_CHARS: usize = 256;
pub const MAX_GROUP_DEPTH: u32 = 8;
/// 一條指令最多對幾個 pattern 做 regex 比對（每個都要跑一輪 NFA；多出來的不比對、放行並記 log）。**shim 同一個數字。**
pub const MAX_REGEX_PATTERNS: usize = 16;

/// 命中 `node` 時多講的一段：agent 最常被這個擋到，而且看不出為什麼（`pkill -x node` 看起來很窄）。
pub const NODE_NOTE: &str = "；`node` 也是 claude／codex（npm 安裝版）的行程名，殺掉同名行程會連 agent 自己一起殺掉。要停自己的 node 服務，請改用自己記下的 PID 或 port：`lsof -ti :PORT | xargs kill`，或 `kill <PID>`";

pub const HOW_TO: &str = "只殺自己記下的 PID 或 port：`lsof -ti :PORT | xargs kill`，或 `kill <自己啟動時記下的 PID>`。不要用 pkill／killall 比對寬鬆的字串。";

/// 一次判斷的共用狀態：「判不出來所以放行」的原因（給 hook 寫 stderr）。
struct Ctx {
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
    let mut ctx = Ctx { notes: Vec::new() };
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
    let mut shell: Option<ShellOpts> = None;
    let verdict = match name {
        "kill" => check_kill(args),
        "pkill" => check_pkill(args, ctx),
        "killall" => check_killall(args, ctx),
        n if is_shell(n) => {
            let o = shell_opts(args);
            let verdict = match o.command.and_then(|i| args.get(i)) {
                // `bash -n`／`-o noexec`：只檢查語法、不執行，裡面寫什麼都不會跑。
                Some(_) | None if o.noexec => Verdict::Allow,
                // `bash -c '…'`、`bash -lc '…'`：第一個非選項字是一段 shell 指令。
                Some(script) => check_depth(script, depth + 1, ctx),
                None => Verdict::Allow,
            };
            shell = Some(o);
            verdict
        }
        "eval" => check_depth(&args.join(" "), depth + 1, ctx),
        _ => Verdict::Allow,
    };
    if let Verdict::Deny(r) = verdict {
        return Verdict::Deny(r);
    }
    // heredoc／here-string：餵給 shell（沒有 -c、沒有 script 檔參數）的就是一份要跑的 script；其他指令的未加引號 heredoc 內文只有 `$(…)`、反引號會被執行。
    for doc in &simple.docs {
        // 餵給 shell 當 script：沒有 `-c`、沒有 script 檔參數（或有 `-s`）、不是 noexec。
        let as_script = shell.as_ref().is_some_and(|o| !o.noexec && o.command.is_none() && (o.stdin_script || o.positional >= args.len()));
        let v = if as_script {
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

/// shell 命令列的選項照順序走完的結果。
struct ShellOpts {
    /// 最後是不是 noexec（`-n`、`-o noexec` 開、`+n`、`+o noexec` 關，照順序）。選項裡有不認得的就是 `false`（判斷不了，不當 noexec 放行）。
    noexec: bool,
    /// `-c` 的指令字在 `args` 的位置（第一個非選項字）。
    command: Option<usize>,
    /// 有 `-s`：script 從 stdin 讀，後面的字是位置參數。
    stdin_script: bool,
    /// 第一個非選項字的位置（沒有就是 `args.len()`）。
    positional: usize,
}

/// 認得的不帶值的單字母選項（bash／sh／dash／zsh／ksh 共有或常見的）；不在這裡的選項字母＝判斷不了。
const SHELL_FLAGS: &str = "abefhikmnprstuvxBCDEHIJLPTclsX";
/// 認得的不帶值的長選項（bash）。
const SHELL_LONG_FLAGS: &[&str] = &["posix", "noediting", "noprofile", "norc", "restricted", "verbose", "login", "debugger", "dump-strings", "dump-po-strings", "help", "version"];

/// 把 `args`（shell 後面的參數）當 shell 的命令列選項照順序走完：`-o`／`+o`／`-O`／`+O` 與 `--rcfile`／`--init-file` 帶值、
/// 要把值一起吃掉，不然 `bash -n -O extglob +n -c '…'` 會在 `extglob` 停下、把前面的 `-n` 當成最後狀態。
/// 遇到不認得的選項（字母、長選項、`-o` 的值不是 `noexec`）：整個 `noexec` 當 `false`（照一般規則判斷，不放行）。
fn shell_opts(args: &[String]) -> ShellOpts {
    let mut o = ShellOpts { noexec: false, command: None, stdin_script: false, positional: args.len() };
    let (mut noexec, mut c, mut i) = (false, false, 0usize);
    // `unsure`：看不懂的選項（字母、長選項），分不出哪個字是值；`cancel`：選項都看得懂，只是 `-o` 的值不是 noexec——只取消 noexec，位置照實。
    let (mut unsure, mut cancel) = (false, false);
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let on = a.starts_with('-');
        if !(on || a.starts_with('+')) || a.len() < 2 {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "rcfile" | "init-file" => i += 2,
                l if SHELL_LONG_FLAGS.contains(&l) => i += 1,
                _ => {
                    unsure = true;
                    i += 1;
                }
            }
            continue;
        }
        let mut consumed = 1;
        for ch in a[1..].chars() {
            match ch {
                'n' => noexec = on,
                'c' => c = on,
                's' => o.stdin_script = on,
                'o' => {
                    // 只有明確認得的 `noexec`（zsh 不分大小寫與底線：`NO_EXEC`、`no_exec`）才影響 noexec 狀態；
                    // 其他值（含 zsh 的 `exec`／`+o exec`、不認得的）一律取消快速放行、走一般判斷。
                    match args.get(i + consumed).map(|v| v.to_ascii_lowercase().replace('_', "")) {
                        Some(v) if v == "noexec" => noexec = on,
                        _ => cancel = true,
                    }
                    consumed += 1;
                }
                'O' => {
                    // bash 的 `-O` 是 shopt 名（`extglob`…），不影響 noexec；但 `exec` 這個名字在別的 shell 可能是執行開關，保守起見也取消。
                    if args.get(i + consumed).is_some_and(|v| v.to_ascii_lowercase().replace('_', "") == "exec") {
                        cancel = true;
                    }
                    consumed += 1;
                }
                x if SHELL_FLAGS.contains(x) => {}
                _ => unsure = true,
            }
        }
        i += consumed;
    }
    o.positional = i.min(args.len());
    if c {
        o.command = (i < args.len()).then_some(i);
        if o.command.is_some() {
            o.positional = i + 1;
        }
    }
    if unsure {
        // 選項看不懂就看不準哪個字是值：指令字與 script 檔參數退回粗略的找法（含 `c` 的短選項叢的下一個字；第一個不以 `-`／`+` 開頭的字），
        // 而且絕不當 noexec 放行。
        o.command = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c')).map(|p| p + 1).filter(|p| *p < args.len());
        o.positional = args.iter().position(|a| !a.starts_with(['-', '+'])).unwrap_or(args.len());
    }
    o.noexec = noexec && !unsure && !cancel;
    o
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
    for (i, pat) in p.patterns.iter().enumerate() {
        if let Some(why) = broad_pattern(pat, full, exact, icase, i < MAX_REGEX_PATTERNS, ctx) {
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
    for (i, name) in p.patterns.iter().enumerate() {
        let hit = if regex {
            broad_pattern(name, false, false, icase, i < MAX_REGEX_PATTERNS, ctx)
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
fn broad_pattern(pat: &str, full: bool, exact: bool, icase: bool, regex_ok: bool, ctx: &mut Ctx) -> Option<String> {
    // 以位元組計：shim 的 `${#pat}` 在 C locale 也是位元組，兩邊同一個數字（非 ASCII 的 pattern 反正在可判斷語法之外）。
    if !exact && pat.len() < MIN_PATTERN_CHARS {
        return Some(format!("只有 {} 個字元，會命中大量不相干的行程（例如命令列含該字串的所有 agent）", pat.len()));
    }
    let shown = || pat.chars().take(40).collect::<String>();
    if !regex_ok {
        ctx.note(format!("pattern `{}` 超過一條指令最多比對 {MAX_REGEX_PATTERNS} 個 pattern 的上限，放行", shown()));
        return None;
    }
    let compiled = match compile_pattern(pat) {
        Ok(c) => c,
        Err(why) => {
            // 判不出來不是「不命中」：放行，並留下原因。
            ctx.note(format!("pattern `{}` {why}，放行", shown()));
            return None;
        }
    };
    let subjects: &[&str] = if full { PROTECTED_CMDLINES } else { PROTECTED_NAMES };
    let hit = subjects.iter().copied().find(|s| compiled.is_match(s, icase, exact))?;
    let name = if full { hit.split_whitespace().next().map(basename).unwrap_or(hit) } else { hit };
    let note = if name == "node" { NODE_NOTE } else { "" };
    Some(format!("會命中受保護的 agent 行程（`{name}`）{note}"))
}

// ───────────────────────── 迷你 ERE ─────────────────────────
// pkill 的 pattern 是 ERE，agent 隨手寫、不能信任。**只在「可判斷語法」裡比對**（[`in_common_subset`]）：
// 這個子集合在 Rust 這邊與 PATH 上 shim 的 `grep -E`（POSIX ERE，GNU 與 BSD 都一樣）語意完全相同，
// 所以兩邊對同一條指令給同一個答案；子集合以外（壞語法、`{n,m}`、`\d`、反向參照、`[[:alpha:]]`、過深的巢狀、非 ASCII…）
// 兩邊都**判不出來**：放行，並記 log。判不出來不是「不命中」——寧可放行，不擋掉正常指令。
//
// 比對本身是 Thompson NFA 模擬（時間 O(pattern × 文字)，沒有回溯），所以不需要步數預算：子集合裡的任何 pattern 都在固定上限內跑完。
//
// 可判斷語法（`alt := seq ('|' seq)*`、`seq := item+`、`item := atom quant?`）：
// * atom：可印 ASCII 字面字元（`. * + ? | ( ) [ ] ^ $ { } \` 以外）、`.`、`\` 加上述任一個 metachar（字面字元）、
//   括號群組（巢狀 ≤ [`MAX_GROUP_DEPTH`]，不可空）、字元集 `[abc0-9_]`／`[^…]`（內容只有英數底線空白 `. / : , @ =`，
//   範圍只准 `0-9`、`a-z`、`A-Z`，至少一項）；
// * quant：`*`、`+`、`?` 各一個，只接在 atom 後面，不可疊（`a**`）、不可接在開頭／`(`／`|` 後面；
// * `^` 只在最外層、某個 seq 的最前面；`$` 只在最外層、某個 seq 的最後面；
// * 沒有空的 seq（`a|`、`()`、`(|a)`）、括號必須成對、長度 ≤ [`MAX_PATTERN_CHARS`]。

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

/// 這個 pattern 在不在「可判斷語法」裡。**sh shim 的 `am_in_subset` 是同一條規則的 sh 版**，兩邊靠 parity 測試保持一致。
pub fn in_common_subset(pat: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Prev {
        SeqStart,
        Atom,
        Quant,
        Bol,
        Eol,
    }
    let c: Vec<char> = pat.chars().collect();
    if c.is_empty() || c.len() > MAX_PATTERN_CHARS {
        return false;
    }
    let (mut i, mut depth, mut prev) = (0usize, 0u32, Prev::SeqStart);
    const META: &str = ".*+?|()[]^${}\\";
    while i < c.len() {
        let ch = c[i];
        if prev == Prev::Eol && ch != '|' && ch != ')' {
            return false;
        }
        match ch {
            '\\' => {
                match c.get(i + 1) {
                    Some(n) if META.contains(*n) => i += 1,
                    _ => return false,
                }
                prev = Prev::Atom;
            }
            '[' => {
                i += 1;
                if c.get(i) == Some(&'^') {
                    i += 1;
                }
                let mut tokens = 0;
                loop {
                    let Some(&x) = c.get(i) else { return false };
                    if x == ']' {
                        break;
                    }
                    let is_range = matches!((x, c.get(i + 1), c.get(i + 2)), ('0', Some('-'), Some('9')) | ('a', Some('-'), Some('z')) | ('A', Some('-'), Some('Z')));
                    if is_range {
                        i += 3;
                    } else if x.is_ascii_alphanumeric() || " _./:,@=".contains(x) {
                        i += 1;
                    } else {
                        return false;
                    }
                    tokens += 1;
                }
                if tokens == 0 {
                    return false;
                }
                prev = Prev::Atom;
            }
            '(' => {
                depth += 1;
                if depth > MAX_GROUP_DEPTH {
                    return false;
                }
                prev = Prev::SeqStart;
            }
            ')' => {
                if depth == 0 || !matches!(prev, Prev::Atom | Prev::Quant) {
                    return false;
                }
                depth -= 1;
                prev = Prev::Atom;
            }
            '|' => {
                if !matches!(prev, Prev::Atom | Prev::Quant | Prev::Eol) {
                    return false;
                }
                prev = Prev::SeqStart;
            }
            '*' | '+' | '?' => {
                if prev != Prev::Atom {
                    return false;
                }
                prev = Prev::Quant;
            }
            '^' => {
                if depth != 0 || prev != Prev::SeqStart {
                    return false;
                }
                prev = Prev::Bol;
            }
            '$' => {
                if depth != 0 || !matches!(prev, Prev::Atom | Prev::Quant | Prev::Bol) {
                    return false;
                }
                prev = Prev::Eol;
            }
            '{' | '}' | ']' => return false,
            '.' => prev = Prev::Atom,
            x if (' '..='~').contains(&x) => prev = Prev::Atom,
            _ => return false,
        }
        i += 1;
    }
    depth == 0 && matches!(prev, Prev::Atom | Prev::Quant | Prev::Eol)
}

fn parse_alt(c: &[char], i: &mut usize, nested: u32) -> Option<Vec<Vec<Item>>> {
    let mut alts = vec![Vec::new()];
    while *i < c.len() {
        let ch = c[*i];
        *i += 1;
        let atom = match ch {
            '|' => {
                alts.push(Vec::new());
                continue;
            }
            ')' if nested > 0 => return Some(alts),
            '(' => Atom::Group(parse_alt(c, i, nested + 1)?),
            '.' => Atom::Any,
            '^' => Atom::Bol,
            '$' => Atom::Eol,
            '\\' => {
                let e = *c.get(*i)?;
                *i += 1;
                Atom::Lit(e)
            }
            '[' => {
                let neg = c.get(*i) == Some(&'^');
                if neg {
                    *i += 1;
                }
                let mut ranges = Vec::new();
                loop {
                    let x = *c.get(*i)?;
                    *i += 1;
                    if x == ']' {
                        break;
                    }
                    if c.get(*i) == Some(&'-') && c.get(*i + 1).is_some_and(|&y| y != ']') {
                        ranges.push((x, c[*i + 1]));
                        *i += 2;
                    } else {
                        ranges.push((x, x));
                    }
                }
                Atom::Class(ranges, neg)
            }
            '*' | '+' | '?' => return None,
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
        alts.last_mut()?.push(Item { atom, min, max });
    }
    if nested > 0 { None } else { Some(alts) }
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

enum Inst {
    Atom(Atom),
    Split(usize, usize),
    Jmp(usize),
    Bol,
    Eol,
    Match,
}

fn emit_alts(prog: &mut Vec<Inst>, alts: &[Vec<Item>]) {
    let mut jmps = Vec::new();
    for (i, alt) in alts.iter().enumerate() {
        if i + 1 < alts.len() {
            let split = prog.len();
            prog.push(Inst::Split(split + 1, 0));
            alt.iter().for_each(|it| emit_item(prog, it));
            jmps.push(prog.len());
            prog.push(Inst::Jmp(0));
            let next = prog.len();
            prog[split] = Inst::Split(split + 1, next);
        } else {
            alt.iter().for_each(|it| emit_item(prog, it));
        }
    }
    let end = prog.len();
    for j in jmps {
        prog[j] = Inst::Jmp(end);
    }
}

fn emit_atom(prog: &mut Vec<Inst>, atom: &Atom) {
    match atom {
        Atom::Group(alts) => emit_alts(prog, alts),
        Atom::Bol => prog.push(Inst::Bol),
        Atom::Eol => prog.push(Inst::Eol),
        a => prog.push(Inst::Atom(a.clone())),
    }
}

fn emit_item(prog: &mut Vec<Inst>, item: &Item) {
    match (item.min, item.max) {
        (0, 1) => {
            let s = prog.len();
            prog.push(Inst::Split(s + 1, 0));
            emit_atom(prog, &item.atom);
            let e = prog.len();
            prog[s] = Inst::Split(s + 1, e);
        }
        (0, usize::MAX) => {
            let s = prog.len();
            prog.push(Inst::Split(s + 1, 0));
            emit_atom(prog, &item.atom);
            prog.push(Inst::Jmp(s));
            let e = prog.len();
            prog[s] = Inst::Split(s + 1, e);
        }
        (1, usize::MAX) => {
            let s = prog.len();
            emit_atom(prog, &item.atom);
            let sp = prog.len();
            prog.push(Inst::Split(s, sp + 1));
        }
        _ => emit_atom(prog, &item.atom),
    }
}

/// 編好的 pattern。`Err`＝在可判斷語法之外（呼叫端放行並記 log）。
pub struct Compiled(Vec<Inst>);

pub fn compile_pattern(pat: &str) -> Result<Compiled, &'static str> {
    if !in_common_subset(pat) {
        return Err("超出可判斷的 regex 語法範圍");
    }
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let Some(alts) = parse_alt(&chars, &mut i, 0) else { return Err("超出可判斷的 regex 語法範圍") };
    let mut prog = Vec::new();
    emit_alts(&mut prog, &alts);
    prog.push(Inst::Match);
    Ok(Compiled(prog))
}

/// 把 `pc` 的 ε 閉包（含 `^`、`$` 在這個位置成不成立）加進 `set`；`mark[pc] == gen` 表示這一輪已經加過。
fn add_state(prog: &[Inst], mark: &mut [usize], gen: usize, set: &mut Vec<usize>, pc: usize, pos: usize, len: usize) {
    let mut stack = vec![pc];
    while let Some(pc) = stack.pop() {
        if mark[pc] == gen {
            continue;
        }
        mark[pc] = gen;
        match &prog[pc] {
            Inst::Jmp(t) => stack.push(*t),
            Inst::Split(a, b) => {
                stack.push(*b);
                stack.push(*a);
            }
            Inst::Bol => {
                if pos == 0 {
                    stack.push(pc + 1)
                }
            }
            Inst::Eol => {
                if pos == len {
                    stack.push(pc + 1)
                }
            }
            Inst::Atom(_) | Inst::Match => set.push(pc),
        }
    }
}

impl Compiled {
    /// `exact`＝`pkill -x`（整串都要符合）；否則是子字串搜尋（`pkill`／`pgrep` 的預設）。
    pub fn is_match(&self, text: &str, icase: bool, exact: bool) -> bool {
        let prog = &self.0;
        let t: Vec<char> = text.chars().collect();
        let mut mark = vec![usize::MAX; prog.len()];
        let (mut cur, mut next): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        add_state(prog, &mut mark, 0, &mut cur, 0, 0, t.len());
        for pos in 0..=t.len() {
            if pos > 0 && !exact {
                add_state(prog, &mut mark, pos, &mut cur, 0, pos, t.len());
            }
            if cur.iter().any(|&pc| matches!(prog[pc], Inst::Match)) && (!exact || pos == t.len()) {
                return true;
            }
            if pos == t.len() {
                break;
            }
            next.clear();
            for &pc in &cur {
                if let Inst::Atom(a) = &prog[pc] {
                    if atom_one(a, t[pos], icase) {
                        add_state(prog, &mut mark, pos + 1, &mut next, pc + 1, pos + 1, t.len());
                    }
                }
            }
            std::mem::swap(&mut cur, &mut next);
        }
        false
    }
}

/// 獨立一次比對（測試用）：超出可判斷語法當作「不命中」。
#[doc(hidden)]
pub fn regex_match(pat: &str, text: &str, icase: bool, exact: bool) -> bool {
    compile_pattern(pat).map(|c| c.is_match(text, icase, exact)).unwrap_or(false)
}

/// 同 [`regex_match`]，但把「判不出來」跟「不命中」分開（測試用）。
#[doc(hidden)]
pub fn regex_match_limited(pat: &str, text: &str, icase: bool, exact: bool) -> Result<bool, &'static str> {
    compile_pattern(pat).map(|c| c.is_match(text, icase, exact))
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
    /// 是 PreToolUse，要擋：stdout 印這段決策 JSON（claude／codex／grok 的 `hookSpecificOutput` 形狀；agy 只印 `decision`／`reason`）。
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
    // agy 1.4.7 只認 `decision`／`reason`；`hookSpecificOutput` 是未知欄位，protojson 會拒絕整個結果、理由也被蓋掉（SPEC agy）。
    if provider == "agy" {
        return Guard::Deny(json!({"decision": "deny", "reason": reason}).to_string());
    }
    let out = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    Guard::Deny(out.to_string())
}
