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
const PROTECTED_NAMES: &[&str] = &["claude", "codex", "grok", "herdr", "agents-managerd", "agy", "antigravity", "node", "sh", "bash", "zsh", "sshd"];

/// `pkill -f` 比對的是整條命令列：用各 agent 實際長相的樣本去試 pattern 會不會命中。
const PROTECTED_CMDLINES: &[&str] = &[
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

const HOW_TO: &str = "只殺自己記下的 PID 或 port：`lsof -ti :PORT | xargs kill`，或 `kill <自己啟動時記下的 PID>`。不要用 pkill／killall 比對寬鬆的字串。";

/// 一整條 shell 指令（可含 `;`、`&&`、`|`、`$(…)`、`bash -c '…'`）。第一個要擋的 segment 決定原因。
pub fn check_command(cmd: &str) -> Verdict {
    check_depth(cmd, 0)
}

fn check_depth(cmd: &str, depth: u32) -> Verdict {
    if depth > MAX_DEPTH {
        return Verdict::Allow;
    }
    for sub in substitutions(cmd) {
        if let Verdict::Deny(r) = check_depth(&sub, depth + 1) {
            return Verdict::Deny(r);
        }
    }
    for words in split_commands(cmd) {
        if let Verdict::Deny(r) = check_words(&words, depth) {
            return Verdict::Deny(r);
        }
    }
    Verdict::Allow
}

fn check_words(words: &[String], depth: u32) -> Verdict {
    let w = unwrap_wrappers(words);
    let Some(head) = w.first() else { return Verdict::Allow };
    let args = &w[1..];
    match basename(head) {
        "kill" => check_kill(args),
        "pkill" => check_pkill(args),
        "killall" => check_killall(args),
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            // `bash -c '…'`、`bash -lc '…'`：-c 的下一個參數是一段 shell 指令。
            let c = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'));
            match c.and_then(|i| args.get(i + 1)) {
                Some(script) => check_depth(script, depth + 1),
                None => Verdict::Allow,
            }
        }
        "eval" => check_depth(&args.join(" "), depth + 1),
        _ => Verdict::Allow,
    }
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

/// 把 `VAR=x`、`sudo -u u`、`env -i`、`nohup`、`timeout 5`、`xargs -n1`… 這些前綴剝掉，回真正的指令字起頭。
fn unwrap_wrappers(mut w: &[String]) -> &[String] {
    loop {
        let Some(head) = w.first() else { return w };
        if is_assignment(head) || matches!(head.as_str(), "!" | "{" | "then" | "do" | "else" | "elif" | "if" | "while" | "until") {
            w = &w[1..];
            continue;
        }
        // 各 wrapper 帶值的選項（其餘選項視為無值旗標）。
        let valued: &[&str] = match basename(head) {
            "sudo" | "doas" => &["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T", "-U"],
            "env" => &["-u", "-C", "-S"],
            "nice" => &["-n"],
            "ionice" => &["-c", "-n", "-p"],
            "timeout" => &["-s", "-k"],
            "xargs" => &["-n", "-I", "-P", "-L", "-d", "-E", "-s", "-a", "-J"],
            "command" | "exec" | "builtin" | "nohup" | "time" | "setsid" | "stdbuf" => &[],
            _ => return w,
        };
        let name = basename(head);
        let mut i = 1;
        while i < w.len() {
            let a = w[i].as_str();
            if a == "--" {
                i += 1;
                break;
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
            return &w[w.len()..];
        }
        w = &w[i..];
    }
}

// ───────────────────────── 切指令 ─────────────────────────

/// 依 `; & | \n ( )` 與引號切成多條簡單指令，每條是去引號後的字。`# 註解` 到行尾略過。解析不了就當作沒有（呼叫端放行）。
fn split_commands(cmd: &str) -> Vec<Vec<String>> {
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let chars: Vec<char> = cmd.chars().collect();
    let (mut i, mut single, mut double) = (0, false, false);
    let flush = |cur: &mut String, has: &mut bool, words: &mut Vec<String>| {
        if *has {
            words.push(std::mem::take(cur));
            *has = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
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
                ' ' | '\t' | '\r' => flush(&mut cur, &mut has, &mut words),
                // `2>&1`、`&>file`、`>&2` 的 `&` 是重導向的一部分，不是背景符號。
                '&' if cur.ends_with(['>', '<']) || chars.get(i + 1) == Some(&'>') => {
                    cur.push(c);
                    has = true;
                }
                ';' | '&' | '|' | '\n' | '(' | ')' | '`' => {
                    flush(&mut cur, &mut has, &mut words);
                    if !words.is_empty() {
                        cmds.push(std::mem::take(&mut words));
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
    flush(&mut cur, &mut has, &mut words);
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds.into_iter().map(strip_redirects).filter(|w| !w.is_empty()).collect()
}

/// 重導向（`2>/dev/null`、`> out`、`2>&1`、`<in`）不是參數：連同它的目標檔一起拿掉。
fn strip_redirects(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
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
    out
}

/// `$(…)` 與反引號裡的指令（雙引號裡也會執行；單引號裡不會）。巢狀的由遞迴再拆。
fn substitutions(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut single) = (0, false);
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            single = !single;
        } else if !single && c == '\\' {
            i += 1;
        } else if !single && c == '$' && chars.get(i + 1) == Some(&'(') {
            let (mut depth, mut j) = (1, i + 2);
            while j < chars.len() && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let end = if depth == 0 { j - 1 } else { j };
            out.push(chars[i + 2..end].iter().collect());
        } else if !single && c == '`' {
            if let Some(len) = chars[i + 1..].iter().position(|&x| x == '`') {
                out.push(chars[i + 1..i + 1 + len].iter().collect());
                i += len + 1;
            }
        }
        i += 1;
    }
    out
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

const SIGNAL_NAMES: &[&str] = &["HUP", "INT", "QUIT", "KILL", "TERM", "USR1", "USR2", "STOP", "CONT", "ALRM", "TSTP", "PIPE", "ABRT"];

fn pkill_valued_short(c: char) -> bool {
    "PugGstTFJjMN".contains(c)
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

fn check_pkill(args: &[String]) -> Verdict {
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
        if let Some(why) = broad_pattern(pat, full, exact, icase) {
            return Verdict::Deny(format!("擋下 pkill：比對字串 `{pat}` {why}。{HOW_TO}"));
        }
    }
    Verdict::Allow
}

fn check_killall(args: &[String]) -> Verdict {
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
    let (regex, icase) = (p.has("r") || p.has("regexp"), p.has("I") || p.has("ignore-case"));
    if p.patterns.is_empty() {
        if p.value(&["u", "user", "t", "c"]).is_some() {
            return Verdict::Deny(format!("擋下 killall：沒有行程名稱、只用 -u／-t／-c 條件，會殺掉那個使用者／終端的所有行程（含其他 agent）。{HOW_TO}"));
        }
        return Verdict::Allow;
    }
    for name in &p.patterns {
        let hit = if regex {
            broad_pattern(name, false, false, icase)
        } else {
            PROTECTED_NAMES.iter().find(|n| eq(n, name, icase)).map(|n| format!("就是受保護的行程名 `{n}`（killall 會殺掉同名的全部行程，包含其他 agent 與使用者的 session）"))
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
fn broad_pattern(pat: &str, full: bool, exact: bool, icase: bool) -> Option<String> {
    if !exact && pat.chars().count() < MIN_PATTERN_CHARS {
        return Some(format!("只有 {} 個字元，會命中大量不相干的行程（例如命令列含該字串的所有 agent）", pat.chars().count()));
    }
    let subjects: &[&str] = if full { PROTECTED_CMDLINES } else { PROTECTED_NAMES };
    let hit = subjects.iter().find(|s| regex_match(pat, s, icase, exact))?;
    let name = if full { hit.split_whitespace().next().map(basename).unwrap_or(hit) } else { hit };
    Some(format!("會命中受保護的 agent 行程（`{name}`）"))
}

// ───────────────────────── 迷你 ERE ─────────────────────────
// pkill 的 pattern 是 ERE。這裡只需要「這個 pattern 會不會命中這個樣本」：支援 `. * + ? | ( ) [ ] ^ $ \x`，
// 其他（`{n,m}`、反向參照…）一律當字面字元；pattern 語法壞掉就退回「樣本含有這串字」的子字串判斷。

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

fn parse_alt(c: &[char], i: &mut usize, nested: bool) -> Option<Vec<Vec<Item>>> {
    let mut alts = vec![Vec::new()];
    while *i < c.len() {
        let ch = c[*i];
        *i += 1;
        let atom = match ch {
            '|' => {
                alts.push(Vec::new());
                continue;
            }
            ')' if nested => return Some(alts),
            '(' => Atom::Group(parse_alt(c, i, true)?),
            '.' => Atom::Any,
            '^' => Atom::Bol,
            '$' => Atom::Eol,
            '\\' => {
                let e = *c.get(*i)?;
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
                    let x = *c.get(*i)?;
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
            '*' | '+' | '?' => return None, // 前面沒有東西可重複
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
    if nested { None } else { Some(alts) }
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

fn match_seq(seq: &[Item], text: &[char], pos: usize, icase: bool, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let Some((item, rest)) = seq.split_first() else { return k(pos) };
    match &item.atom {
        Atom::Bol => pos == 0 && match_seq(rest, text, pos, icase, k),
        Atom::Eol => pos == text.len() && match_seq(rest, text, pos, icase, k),
        Atom::Group(alts) => {
            fn rep(item: &Item, alts: &[Vec<Item>], rest: &[Item], text: &[char], pos: usize, n: usize, icase: bool, k: &mut dyn FnMut(usize) -> bool) -> bool {
                if n < item.max {
                    for alt in alts {
                        let mut next = |p: usize| (p != pos || n < item.min) && rep(item, alts, rest, text, p, n + 1, icase, k);
                        if match_seq(alt, text, pos, icase, &mut next) {
                            return true;
                        }
                    }
                }
                n >= item.min && match_seq(rest, text, pos, icase, k)
            }
            rep(item, alts, rest, text, pos, 0, icase, k)
        }
        atom => {
            let mut n = 0;
            while n < item.max && pos + n < text.len() && atom_one(atom, text[pos + n], icase) {
                n += 1;
            }
            loop {
                if n >= item.min && match_seq(rest, text, pos + n, icase, k) {
                    return true;
                }
                if n == 0 || n <= item.min {
                    return false;
                }
                n -= 1;
            }
        }
    }
}

/// `exact`＝`pkill -x`（整串都要符合）；否則是子字串搜尋（`pkill`／`pgrep` 的預設）。
#[doc(hidden)]
pub fn regex_match(pat: &str, text: &str, icase: bool, exact: bool) -> bool {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let Some(alts) = parse_alt(&chars, &mut i, false) else {
        let (p, t) = if icase { (pat.to_ascii_lowercase(), text.to_ascii_lowercase()) } else { (pat.to_string(), text.to_string()) };
        return if exact { p == t } else { t.contains(&p) };
    };
    let t: Vec<char> = text.chars().collect();
    let starts = if exact { 0..=0 } else { 0..=t.len() };
    for s in starts {
        for alt in &alts {
            let mut k = |end: usize| !exact || end == t.len();
            if match_seq(alt, &t, s, icase, &mut k) {
                return true;
            }
        }
    }
    false
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

pub fn guard(provider: &str, event: &str, stdin_text: &str) -> Guard {
    let Ok(payload) = serde_json::from_str::<Value>(stdin_text) else { return if event_is_pre(event) { Guard::Pass } else { Guard::NotPreToolUse } };
    if !is_pre_tool_use(event, &payload) {
        return Guard::NotPreToolUse;
    }
    let Some(command) = command_from_payload(&payload) else { return Guard::Pass };
    let Verdict::Deny(reason) = check_command(&command) else { return Guard::Pass };
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
