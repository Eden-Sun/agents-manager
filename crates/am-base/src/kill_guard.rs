//! 廣域 kill 守門（issue #1884）：PreToolUse hook 在 bot 的 shell 指令跑之前判斷「這條會不會殺一大片行程」。
//!
//! 事故：`pkill -f "next-server" -P 1`——macOS／BSD 的 `pkill` 遇到第一個非選項參數就停止解析選項，`-P 1` 被當成**額外的比對字串**，
//! 結果殺掉命令列含「1」的全部行程（同一台機器上所有 claude／codex／grok 與 Chrome）。
//!
//! 全部是**靜態判斷**：只看指令字串，不跑任何外部指令（不 `pgrep`、不 `ps`），所以不會慢、不會卡住、不會因環境不同而結果不同。
//! 指令本身解析不了（引號沒閉合、巢狀超過上限、`env -S` 展開過多）就放行並記 log——這個 hook 絕不能因為自己的錯擋掉正常指令；
//! 但解析得了的 `kill`／`xargs kill`，目標判不出是哪一個行程就擋。SPEC「廣域 kill 守門」是權威來源。
//!
//! 政策（使用者 2026-11 裁示，全域硬規則）：
//! * `pkill`、`killall` 一律擋（不論選項、比對字串，`pkill -f 'scripts/check.sh changed'` 這種窄的也會命中自己的 shell；`pgrep … | xargs kill` 同樣）；
//! * `kill` 只放行明確 PID：數字（可多個）與 `$!`；其他目標（變數、`$(…)`、`%1`）判不出是哪一個行程，擋；訊號 0（`kill -0 "$pid"`，只探測、不送訊號）不受此限；
//! * `xargs kill` 只准緊接在 `lsof -t… -i:PORT` 後面（`lsof -ti :PORT | xargs kill`）：必須解析出每個 `-i` 都帶 `:<數字>` 的 port，沒有 port 的 `lsof -ti` 會列出所有持有 socket 的行程；
//! * `kill -1`／`kill -9 -1`／`kill 0`（所有行程／整個行程群組）一律擋。

use serde_json::{json, Value};

/// `Allow`＝放行；`Deny(原因)`＝擋下，原因直接給 agent 看（含正確做法）。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

const MAX_DEPTH: u32 = 4;

/// 政策（一律擋）的訊息：寫明正確做法（啟動時記 PID、收尾 `kill <PID>`；dev server 用 port）。
pub const PKILL_DENY: &str = "擋下 pkill：pkill 一律不准用（不論 -f、-P、-x 或比對字串；窄的比對也會命中自己的 shell、或別的 agent）。\
正確做法：啟動時把 PID 記下來（`cmd & echo $!`，或 `$!` 緊接著用），收尾時 `kill $!` 或 `kill <PID>`；dev server 用 port：`lsof -ti :PORT | xargs kill`。";
pub const KILLALL_DENY: &str = "擋下 killall：killall 一律不准用（會殺掉同名的全部行程，包含其他 agent）。\
正確做法：啟動時把 PID 記下來（`cmd & echo $!`），收尾時 `kill <PID>`；dev server 用 port：`lsof -ti :PORT | xargs kill`。";
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

/// `lsof` 的參數是不是「只列出某幾個 port 的行程 PID」：要有 `-t`，每個 `-i` 都帶 `:<數字>`（`-i:3000`、`-ti:3000`、`-i :3000`、`-i tcp:3000`），
/// 其餘只准 `-n`／`-P`／`-a` 這類不擴大範圍的旗標。沒有 port 的 `-i`、`-p`／`-u`／`-c` 條件、檔名參數（lsof 預設是 OR，會多列出別的行程）一律不算。
fn lsof_port_only(args: &[String]) -> bool {
    let (mut t, mut ports, mut i) = (false, 0, 0);
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty() && !c.starts_with('-')) else { return false };
        for (k, ch) in cluster.char_indices() {
            match ch {
                't' => t = true,
                'n' | 'P' | 'a' => {}
                'i' => {
                    let rest = &cluster[k + 1..];
                    let spec = if rest.is_empty() {
                        let next = args.get(i).map(String::as_str).unwrap_or("");
                        i += 1;
                        next
                    } else {
                        rest
                    };
                    if !is_port_spec(spec) {
                        return false;
                    }
                    ports += 1;
                    break; // `-i` 的值吃掉這個旗標叢的其餘字元
                }
                _ => return false,
            }
        }
    }
    t && ports > 0
}

/// `:3000`、`tcp:3000`、`TCP@localhost:3000`、`4:3000`：最後一個 `:` 後面必須是純數字。
fn is_port_spec(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else { return false };
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && host.chars().all(|c| c.is_ascii_alphanumeric() || "@.-[]".contains(c))
}

/// `xargs … kill`（kill 的 PID 從管線或標準輸入來）：只准緊接在 `lsof -t… -i:PORT` 後面（`lsof -ti :PORT | xargs kill`）。
/// 其他（`pgrep … | xargs kill`、`ps … | xargs kill`…）一律擋。解析只看相鄰的指令，不分 `|` 與 `;`（判斷從嚴：`;` 也要求 lsof 在前）。
fn check_xargs_kill(cmds: &[Simple]) -> Verdict {
    for (i, simple) in cmds.iter().enumerate() {
        let Some(w) = unwrap_wrappers(&simple.words) else { continue };
        let Some(head) = w.first() else { continue };
        let is_xargs = simple.words.iter().find(|x| !is_assignment(x)).is_some_and(|x| basename(x) == "xargs");
        if !(is_xargs && basename(head) == "kill") {
            continue;
        }
        let from_lsof = i > 0 && unwrap_wrappers(&cmds[i - 1].words).is_some_and(|w| w.first().is_some_and(|h| basename(h) == "lsof") && lsof_port_only(&w[1..]));
        if !from_lsof {
            return Verdict::Deny(format!("擋下 kill：`xargs kill` 的 PID 只准來自 `lsof -ti :PORT`（`lsof -ti :PORT | xargs kill`）；pgrep、ps 等比對出來的 PID 一律不准。{HOW_TO}"));
        }
    }
    Verdict::Allow
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
    if let Verdict::Deny(r) = check_xargs_kill(&script.cmds) {
        return Verdict::Deny(r);
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
        "pkill" => check_pkill(),
        "killall" => check_killall(),
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

/// 明確的 PID：數字（含多個），或 `$!`／`${!}`（上一個背景行程）。
fn is_explicit_pid(t: &str) -> bool {
    t == "$!" || t == "${!}" || (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
}

fn check_kill(args: &[String]) -> Verdict {
    let mut targets: Vec<&str> = Vec::new();
    let (mut signal, mut dashdash, mut sig0, mut i) = (false, false, false, 0);
    while i < args.len() {
        let a = args[i].as_str();
        if dashdash {
            targets.push(a);
        } else if a == "--" {
            dashdash = true;
        } else if a == "-l" || a == "-L" || a == "-t" {
            return Verdict::Allow; // 列出訊號名，不殺任何東西
        } else if (a == "-s" || a == "-n") && targets.is_empty() {
            signal = true;
            sig0 = args.get(i + 1).is_some_and(|v| v == "0");
            i += 1;
        } else if !signal && targets.is_empty() && is_signal_spec(a) {
            signal = true;
            sig0 = a == "-0";
        } else {
            targets.push(a);
        }
        i += 1;
    }
    // 訊號 0（`kill -0 "$pid"`）不送任何訊號，只探測行程在不在：目標是什麼都不會殺到誰，放行。
    if sig0 {
        return Verdict::Allow;
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
    // 只放行數字 PID 與 `$!`（上一個背景行程的 PID）。其他（變數、`$(…)`、`%1`…）判不出是哪一個行程，一律擋。
    if let Some(t) = targets.iter().find(|t| !is_explicit_pid(t)) {
        return Verdict::Deny(format!("擋下 kill：目標 `{t}` 不是數字 PID。只准 `kill <數字PID>`（多個也行）或 `kill $!`；要殺的行程請在啟動時就把 PID 記下來（`cmd & echo $!`）。{HOW_TO}"));
    }
    Verdict::Allow
}

// ───────────────────────── pkill／killall ─────────────────────────

fn check_pkill() -> Verdict {
    // 政策（使用者 2026-11 裁示）：pkill 一律擋，不論選項與比對字串（窄的 `pkill -f 'scripts/check.sh changed'` 也會命中自己的 shell）。
    Verdict::Deny(PKILL_DENY.to_string())
}

fn check_killall() -> Verdict {
    // 政策同 pkill：killall 一律擋。
    Verdict::Deny(KILLALL_DENY.to_string())
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
