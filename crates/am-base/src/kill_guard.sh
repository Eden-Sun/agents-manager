#!/bin/sh
AM_PROVIDER=${1:-claude}
case $AM_PROVIDER in
  grok|agy) [ -n "${AM_BOT_ID:-}" ] || { cat >/dev/null 2>&1; exit 0; } ;;
esac
am_unavailable() {
  printf '%s\n' '__UNAVAILABLE__' >&2
  case $AM_PROVIDER in
    agy) printf '%s\n' '{"decision":"deny","reason":"__UNAVAILABLE__"}'; exit 0 ;;
    grok) printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"__UNAVAILABLE__"}}'; exit 0 ;;
  esac
  exit 2
}
AM_OUT=$(head -c 1048576 | LC_ALL=C awk -v provider="$AM_PROVIDER" -v SQ="'" '
function note(m) {
  if (NOTES < 16) { NOTES++; printf "agents-managerd hook: kill_guard: %s\n", m > "/dev/stderr" }
}
function hexval(h,    k, v, d) {
  v = 0
  for (k = 1; k <= 4; k++) {
    d = index("0123456789abcdef", substr(h, k, 1))
    if (d == 0) return -1
    v = v * 16 + d - 1
  }
  return v
}
function jstring(s, i,    L, c, st, buf, nb, k, v, out) {
  L = length(s); st = i; buf = ""; nb = 0; JOK = 0
  while (i <= L) {
    c = substr(s, i, 1)
    if (c == "\"") { buf = buf substr(s, st, i - st); JOK = 1; break }
    if (c == "\\") {
      buf = buf substr(s, st, i - st)
      i++; c = substr(s, i, 1)
      if (c == "n") buf = buf "\n"
      else if (c == "t") buf = buf "\t"
      else if (c == "r") buf = buf "\r"
      else if (c == "b") buf = buf "\b"
      else if (c == "f") buf = buf "\f"
      else if (c == "u") {
        v = hexval(tolower(substr(s, i + 1, 4))); i += 4
        buf = buf ((v > 0 && v < 128) ? sprintf("%c", v) : "?")
      }
      else buf = buf c
      i++; st = i
      if (length(buf) > 4096) { nb++; JB[nb] = buf; buf = "" }
      continue
    }
    i++
  }
  if (!JOK) return ""
  out = ""
  for (k = 1; k <= nb; k++) out = out JB[k]
  return out buf
}
function payload_command(s,    r) {
  if (!match(s, /"([cC][oO][mM][mM][aA][nN][dD]([lL][iI][nN][eE])?|[cC][mM][dD])"[ \t\r\n]*:[ \t\r\n]*"/)) return ""
  FOUND = 1
  r = jstring(s, RSTART + RLENGTH)
  if (!JOK) { FOUND = 0; note("PreToolUse payload 不是合法 JSON，放行"); return "" }
  return r
}
function base(p) { sub(/.*\//, "", p); return p }
function isassign(w) { return w ~ /^[A-Za-z_][A-Za-z0-9_]*=/ }
function flush() {
  if (HAS) { WBN++; WB[WBN] = CUR; CUR = ""; HAS = 0 }
}
function pushraw(D,    j, k) {
  RN[D]++; k = RN[D]
  RWN[D, k] = WBN; RDN[D, k] = 0
  for (j = 1; j <= WBN; j++) RW[D, k, j] = WB[j]
  WBN = 0
}
function btick(n, i,    j) {
  j = i + 1
  while (j <= n) {
    if (CH[j] == "\\") j++
    else if (CH[j] == "`") { PEND = j + 1; return 1 }
    j++
  }
  return 0
}
function dparen(n, i,    j, c, sp, top) {
  sp = 1; STK[1] = "C"; j = i + 2
  while (j <= n) {
    c = CH[j]; top = STK[sp]
    if (top == "S") { if (c == SQ) sp-- }
    else if (top == "D") {
      if (c == "\"") sp--
      else if (c == "\\") j++
      else if (c == "$" && CH[j + 1] == "(") { sp++; STK[sp] = "C"; j++ }
    } else {
      if (c == SQ) { sp++; STK[sp] = "S" }
      else if (c == "\"") { sp++; STK[sp] = "D" }
      else if (c == "\\") j++
      else if (c == "$" && CH[j + 1] == "(") { sp++; STK[sp] = "C"; j++ }
      else if (c == "(") { sp++; STK[sp] = "C" }
      else if (c == ")") sp--
      else if (c == "`") { if (!btick(n, j)) return 0; j = PEND - 1 }
    }
    j++
    if (sp == 0) { PEND = j; return 1 }
  }
  return 0
}
function hdelim(n, j,    c) {
  HSTRIP = (CH[j] == "-"); if (HSTRIP) j++
  while (j <= n && (CH[j] == " " || CH[j] == "\t")) j++
  HD = ""; HQ = 0
  while (j <= n) {
    c = CH[j]
    if (c == SQ || c == "\"") {
      HQ = 1; j++
      while (j <= n && CH[j] != c) { HD = HD CH[j]; j++ }
      j++
    } else if (c == "\\") {
      HQ = 1; j++
      if (j <= n) { HD = HD CH[j]; j++ }
    } else if (c == " " || c == "\t" || c == "\n" || c == ";" || c == "&" || c == "|" || c == "(" || c == ")" || c == "<" || c == ">") break
    else { HD = HD c; j++ }
  }
  HNEXT = (j > n + 1) ? n + 1 : j
  return HD != ""
}
function docs(D, s, n, pos,    p, st, e, line, cmp, body, k, m) {
  for (p = 1; p <= PN; p++) {
    st = pos; body = ""
    while (pos <= n) {
      e = pos
      while (e <= n && CH[e] != "\n") e++
      line = substr(s, pos, e - pos)
      cmp = line
      if (PS[p]) sub(/^\t+/, "", cmp)
      if (cmp == PD[p]) { body = substr(s, st, pos - st); pos = e + 1; break }
      pos = e + 1
      body = substr(s, st, pos - st)
    }
    if (pos > n + 1) pos = n + 1
    k = PO[p]
    if (k <= RN[D]) { RDN[D, k]++; m = RDN[D, k]; RDB[D, k, m] = body; RDX[D, k, m] = PX[p] }
  }
  PN = 0
  return pos
}
function finalize(D,    k, j, w, op, nw, m, kk) {
  CN[D] = 0
  for (k = 1; k <= RN[D]; k++) {
    kk = CN[D] + 1; nw = 0; DN[D, kk] = 0
    for (m = 1; m <= RDN[D, k]; m++) { DN[D, kk]++; DB[D, kk, DN[D, kk]] = RDB[D, k, m]; DX[D, kk, DN[D, kk]] = RDX[D, k, m] }
    for (j = 1; j <= RWN[D, k]; j++) {
      w = RW[D, k, j]
      if (w == HS) {
        if (j < RWN[D, k]) { j++; DN[D, kk]++; DB[D, kk, DN[D, kk]] = RW[D, k, j]; DX[D, kk, DN[D, kk]] = 0 }
        continue
      }
      op = w; sub(/^[0-9]+/, "", op)
      if (substr(op, 1, 1) == ">" || substr(op, 1, 1) == "<" || substr(op, 1, 2) == "&>") {
        if (op == ">" || op == ">>" || op == "<" || op == "&>" || op == "&>>" || op == ">|") j++
        continue
      }
      nw++; CW[D, kk, nw] = w
    }
    if (nw > 0) { CN[D] = kk; CWN[D, kk] = nw }
  }
}
function parse(s, D,    n, i, j, c, sq, dq, lc, ls, failed) {
  n = split(s, CH, "")
  RN[D] = 0; SN[D] = 0; WBN = 0; PN = 0; CUR = ""; HAS = 0
  sq = 0; dq = 0; lc = 0; ls = 0; failed = ""
  i = 1
  while (i <= n) {
    c = CH[i]
    if (!sq && c == "$" && CH[i + 1] == "(") {
      if (!dparen(n, i)) { failed = "`$(` 的括號或引號不成對"; break }
      SN[D]++; SB[D, SN[D]] = substr(s, i + 2, PEND - i - 3)
      CUR = CUR "$(..)"; HAS = 1; i = PEND; continue
    }
    if (!sq && c == "`") {
      if (!btick(n, i)) { failed = "反引號不成對"; break }
      SN[D]++; SB[D, SN[D]] = substr(s, i + 1, PEND - i - 2)
      CUR = CUR "$(..)"; HAS = 1; i = PEND; continue
    }
    if (sq) {
      j = i
      while (j <= n && CH[j] != SQ) j++
      CUR = CUR substr(s, i, j - i)
      if (j <= n) sq = 0
      i = j + 1; continue
    }
    if (dq) {
      if (c == "\"") dq = 0
      else if (c == "\\" && i < n) { i++; CUR = CUR CH[i] }
      else {
        j = i
        while (j <= n && CH[j] != "\"" && CH[j] != "\\" && CH[j] != "$" && CH[j] != "`") j++
        if (j == i) j++
        CUR = CUR substr(s, i, j - i)
        i = j; continue
      }
      i++; continue
    }
    if (c == SQ) { sq = 1; HAS = 1 }
    else if (c == "\"") { dq = 1; HAS = 1 }
    else if (c == "\\" && i < n) { i++; CUR = CUR CH[i]; HAS = 1 }
    else if (c == "#" && !HAS) { while (i <= n && CH[i] != "\n") i++; continue }
    else if (c == " " || c == "\t" || c == "\r") flush()
    else if (c == "<" && CH[i + 1] == "<") {
      flush()
      if (CH[i + 2] == "<") { WBN++; WB[WBN] = HS; i += 3; continue }
      if (hdelim(n, i + 2)) { PN++; PD[PN] = HD; PS[PN] = HSTRIP; PX[PN] = !HQ; PO[PN] = RN[D] + 1; i = HNEXT; continue }
      CUR = CUR c; HAS = 1
    }
    else if (c == "&" && (CUR ~ /[<>]$/ || CH[i + 1] == ">")) { CUR = CUR c; HAS = 1 }
    else if (c == ";" || c == "&" || c == "|" || c == "\n" || c == "(" || c == ")") {
      flush()
      if (WBN > 0) pushraw(D)
      if (c == "\n") {
        i = (PN == 0) ? i + 1 : docs(D, s, n, i + 1)
        lc = RN[D]; ls = SN[D]; continue
      }
    }
    else { CUR = CUR c; HAS = 1 }
    i++
  }
  if (failed == "" && (sq || dq)) failed = "引號沒有閉合"
  if (failed != "") {
    RN[D] = lc; SN[D] = ls; WBN = 0
    if (D != 99) note("指令解析失敗（" failed "），那一行起不判斷、放行")
  } else {
    flush()
    if (WBN > 0) pushraw(D)
  }
  finalize(D)
}
function envsplit(i,    a, cl, k, pre) {
  a = U[i]
  if (substr(a, 1, 15) == "--split-string=") { ESV = substr(a, 16); EUSED = 1; return 1 }
  if (a == "--split-string") { if (i + 1 > UN) return 0; ESV = U[i + 1]; EUSED = 2; return 1 }
  if (substr(a, 1, 1) != "-" || substr(a, 2, 1) == "-") return 0
  cl = substr(a, 2); k = index(cl, "S")
  if (k == 0) return 0
  pre = substr(cl, 1, k - 1)
  gsub(/[i0v]/, "", pre)
  if (pre != "") return 0
  if (k == length(cl)) { if (i + 1 > UN) return 0; ESV = U[i + 1]; EUSED = 2; return 1 }
  ESV = substr(cl, k + 1); EUSED = 1
  return 1
}
function unwrap(D, k,    j, st, h, nm, valued, i, a, ex, again, nt, T2) {
  UN = CWN[D, k]
  for (j = 1; j <= UN; j++) U[j] = CW[D, k, j]
  st = 1; ex = 0
  while (1) {
    if (st > UN) { UON = 0; return 1 }
    h = U[st]
    if (isassign(h) || h == "!" || h == "{" || h == "then" || h == "do" || h == "else" || h == "elif" || h == "if" || h == "while" || h == "until") { st++; continue }
    nm = base(h)
    if (nm == "sudo" || nm == "doas") valued = " -u -g -h -p -C -D -R -T -U "
    else if (nm == "env") valued = " -u -C --unset --chdir "
    else if (nm == "nice") valued = " -n "
    else if (nm == "ionice") valued = " -c -n -p "
    else if (nm == "timeout") valued = " -s -k "
    else if (nm == "xargs") valued = " -n -I -P -L -d -E -s -a -J "
    else if (nm == "command" || nm == "exec" || nm == "builtin" || nm == "nohup" || nm == "time" || nm == "setsid" || nm == "stdbuf") valued = " "
    else break
    i = st + 1; again = 0
    while (i <= UN) {
      a = U[i]
      if (a == "--") { i++; break }
      if (nm == "env" && envsplit(i)) {
        ex++
        if (ex > 16) return 0
        nt = 0
        for (j = i + EUSED; j <= UN; j++) { nt++; T2[nt] = U[j] }
        parse(ESV, 99)
        UN = 0
        if (CN[99] > 0) for (j = 1; j <= CWN[99, 1]; j++) { UN++; U[UN] = CW[99, 1, j] }
        for (j = 1; j <= nt; j++) { UN++; U[UN] = T2[j] }
        st = 1; again = 1
        break
      }
      if (index(valued, " " a " ")) i += 2
      else if ((substr(a, 1, 1) == "-" && length(a) > 1) || (nm == "env" && isassign(a))) i++
      else break
    }
    if (again) continue
    if (nm == "timeout" && i <= UN) i++
    if (i > UN) { UON = 0; return 1 }
    st = i
  }
  UON = 0
  for (j = st; j <= UN; j++) { UON++; UO[UON] = U[j] }
  return 1
}
function portspec(s,    k, p, host, port) {
  p = 0
  for (k = length(s); k >= 1; k--) if (substr(s, k, 1) == ":") { p = k; break }
  if (p == 0) return 0
  host = substr(s, 1, p - 1); port = substr(s, p + 1)
  if (port !~ /^[0-9]+$/) return 0
  gsub(/[A-Za-z0-9@.-]/, "", host); gsub(/\[/, "", host); gsub(/\]/, "", host)
  return host == ""
}
function lsof_ok(    t, ports, i, a, cl, p, ch, rest, spec) {
  t = 0; ports = 0; i = 2
  while (i <= UON) {
    a = UO[i]; i++
    if (substr(a, 1, 1) != "-" || length(a) < 2 || substr(a, 2, 1) == "-") return 0
    cl = substr(a, 2)
    for (p = 1; p <= length(cl); p++) {
      ch = substr(cl, p, 1)
      if (ch == "t") t = 1
      else if (ch == "i") {
        rest = substr(cl, p + 1)
        if (rest == "") { spec = (i <= UON) ? UO[i] : ""; i++ } else spec = rest
        if (!portspec(spec)) return 0
        ports++
        break
      }
      else if (ch != "n" && ch != "P" && ch != "a") return 0
    }
  }
  return t && ports > 0
}
function xargs_check(D,    k, j, hd, fw, from) {
  for (k = 1; k <= CN[D]; k++) {
    if (!unwrap(D, k) || UON == 0) continue
    hd = base(UO[1])
    fw = ""
    for (j = 1; j <= CWN[D, k]; j++) if (!isassign(CW[D, k, j])) { fw = CW[D, k, j]; break }
    if (!(base(fw) == "xargs" && hd == "kill")) continue
    from = 0
    if (k > 1 && unwrap(D, k - 1) && UON > 0 && base(UO[1]) == "lsof" && lsof_ok()) from = 1
    if (!from) return "擋下 kill：`xargs kill` 的 PID 只准來自 `lsof -ti :PORT`（`lsof -ti :PORT | xargs kill`）；pgrep、ps 等比對出來的 PID 一律不准。" HOW_TO
  }
  return ""
}
function sigspec(a,    r) {
  if (substr(a, 1, 1) != "-") return 0
  r = substr(a, 2)
  if (substr(r, 1, 3) == "SIG") r = substr(r, 4)
  return r ~ /^[A-Z0-9]+$/
}
function killcheck(d,    nt, sig, dd, s0, i, a, n) {
  nt = 0; sig = 0; dd = 0; s0 = 0; n = AN[d]
  for (i = 2; i <= n; i++) {
    a = A[d, i]
    if (dd) { nt++; T[nt] = a }
    else if (a == "--") dd = 1
    else if (a == "-l" || a == "-L" || a == "-t") return ""
    else if ((a == "-s" || a == "-n") && nt == 0) { sig = 1; s0 = (i + 1 <= n && A[d, i + 1] == "0"); i++ }
    else if (!sig && nt == 0 && sigspec(a)) { sig = 1; s0 = (a == "-0") }
    else { nt++; T[nt] = a }
  }
  if (s0) return ""
  if (n == 2 && A[d, 2] == "-1") return "擋下廣域 kill：`kill -1` 是 pid -1（這個使用者的所有行程）的寫法。" HOW_TO
  for (i = 1; i <= nt; i++) {
    if (T[i] == "0") return "擋下廣域 kill：目標是 pid 0（呼叫端整個行程群組，包含 agent 自己和上層的 shell）。" HOW_TO
    if (T[i] == "-1") return "擋下廣域 kill：目標是 pid -1（這個使用者的所有行程）。" HOW_TO
  }
  for (i = 1; i <= nt; i++) {
    if (!(T[i] == "$!" || T[i] == "${!}" || T[i] ~ /^[0-9]+$/)) return "擋下 kill：目標 `" T[i] "` 不是數字 PID。只准 `kill <數字PID>`（多個也行）或 `kill $!`；要殺的行程請在啟動時就把 PID 記下來（`cmd & echo $!`）。" HOW_TO
  }
  return ""
}
function arg(d, x) { return A[d, x + 2] }
function shopts(d,    len, i, a, on, lg, consumed, p, ch, v, nx, c, unsure, cancel, noexec) {
  len = AN[d] - 1
  ONX = 0; OCMD = -1; OSTDIN = 0; OPOS = len
  noexec = 0; c = 0; i = 0; unsure = 0; cancel = 0
  while (i < len) {
    a = arg(d, i)
    if (a == "--") { i++; break }
    on = (substr(a, 1, 1) == "-")
    if (!(on || substr(a, 1, 1) == "+") || length(a) < 2) break
    if (substr(a, 1, 2) == "--") {
      lg = substr(a, 3)
      if (lg == "rcfile" || lg == "init-file") i += 2
      else if (index(" posix noediting noprofile norc restricted verbose login debugger dump-strings dump-po-strings help version ", " " lg " ")) i++
      else { unsure = 1; i++ }
      continue
    }
    consumed = 1
    for (p = 2; p <= length(a); p++) {
      ch = substr(a, p, 1)
      if (ch == "n") noexec = on
      else if (ch == "c") c = on
      else if (ch == "s") OSTDIN = on
      else if (ch == "o") {
        nx = i + consumed
        if (nx < len) { v = tolower(arg(d, nx)); gsub(/_/, "", v); if (v == "noexec") noexec = on; else cancel = 1 }
        else cancel = 1
        consumed++
      }
      else if (ch == "O") {
        nx = i + consumed
        if (nx < len) { v = tolower(arg(d, nx)); gsub(/_/, "", v); if (v == "exec") cancel = 1 }
        consumed++
      }
      else if (!index("abefhikmnprstuvxBCDEHIJLPTclsX", ch)) unsure = 1
    }
    i += consumed
  }
  OPOS = (i < len) ? i : len
  if (c) {
    OCMD = (i < len) ? i : -1
    if (OCMD >= 0) OPOS = i + 1
  }
  if (unsure) {
    OCMD = -1
    for (p = 0; p < len; p++) { a = arg(d, p); if (substr(a, 1, 1) == "-" && substr(a, 1, 2) != "--" && index(a, "c")) { OCMD = p + 1; break } }
    if (OCMD >= len) OCMD = -1
    OPOS = len
    for (p = 0; p < len; p++) { a = arg(d, p); if (substr(a, 1, 1) != "-" && substr(a, 1, 1) != "+") { OPOS = p; break } }
  }
  ONX = noexec && !unsure && !cancel
}
function doc_subs_check(body, dep,    n, i, r, c, txt) {
  n = split(body, CH, "")
  NDS = 0
  i = 1
  while (i <= n) {
    c = CH[i]
    if (c == "\\") i++
    else if (c == "$" && CH[i + 1] == "(") {
      if (!dparen(n, i)) break
      NDS++; DSB[dep, NDS] = substr(body, i + 2, PEND - i - 3); i = PEND; continue
    } else if (c == "`") {
      if (!btick(n, i)) break
      NDS++; DSB[dep, NDS] = substr(body, i + 1, PEND - i - 2); i = PEND; continue
    }
    i++
  }
  DSN[dep] = NDS
  for (i = 1; i <= DSN[dep]; i++) { r = check(DSB[dep, i], dep + 1); if (r != "") return r }
  return ""
}
function simple(D, k, dep,    j, nm, r, sh, m, as, nargs, x) {
  if (!unwrap(D, k)) { note("`env -S` 展開次數超過上限，放行"); return "" }
  if (UON == 0) return ""
  AN[dep] = UON
  for (j = 1; j <= UON; j++) A[dep, j] = UO[j]
  nargs = UON - 1
  nm = base(A[dep, 1])
  sh = 0; r = ""
  if (nm == "kill") r = killcheck(dep)
  else if (nm == "pkill") r = PKILL
  else if (nm == "killall") r = KILLALL
  else if (nm == "sh" || nm == "bash" || nm == "zsh" || nm == "dash" || nm == "ksh") {
    shopts(dep)
    sh = 1; SNX[dep] = ONX; SCMD[dep] = OCMD; SSTD[dep] = OSTDIN; SPOS[dep] = OPOS
    if (!ONX && OCMD >= 0) r = check(arg(dep, OCMD), dep + 1)
  }
  else if (nm == "eval") {
    x = ""
    for (j = 2; j <= AN[dep]; j++) x = x (j > 2 ? " " : "") A[dep, j]
    r = check(x, dep + 1)
  }
  if (r != "") return r
  for (m = 1; m <= DN[D, k]; m++) {
    as = sh && !SNX[dep] && SCMD[dep] < 0 && (SSTD[dep] || SPOS[dep] >= nargs)
    if (as) r = check(DB[D, k, m], dep + 1)
    else if (DX[D, k, m]) r = doc_subs_check(DB[D, k, m], dep)
    if (r != "") return r
  }
  return ""
}
function check(cmd, dep,    m, k, r) {
  if (dep > 4) { note("指令巢狀（$()／bash -c／eval）超過上限，放行"); return "" }
  parse(cmd, dep)
  for (m = 1; m <= SN[dep]; m++) { r = check(SB[dep, m], dep + 1); if (r != "") return r }
  r = xargs_check(dep)
  if (r != "") return r
  for (k = 1; k <= CN[dep]; k++) { r = simple(dep, k, dep); if (r != "") return r }
  return ""
}
function jesc(s,    out, i, c, n) {
  out = ""; n = length(s)
  for (i = 1; i <= n; i++) {
    c = substr(s, i, 1)
    if (c == "\\") out = out "\\\\"
    else if (c == "\"") out = out "\\\""
    else if (c in CC) out = out CC[c]
    else out = out c
  }
  return out
}
BEGIN {
  HS = "\002<<<"
  HOW_TO = "__HOW_TO__"
  PKILL = "__PKILL_DENY__"
  KILLALL = "__KILLALL_DENY__"
  for (i = 1; i < 32; i++) CC[sprintf("%c", i)] = sprintf("\\u%04x", i)
}
{ BUF = BUF $0 "\n" }
END {
  cmd = payload_command(BUF)
  r = FOUND ? check(cmd, 0) : ""
  if (r == "") { print "ALLOW"; exit 0 }
  if (provider == "agy") printf "{\"decision\":\"deny\",\"reason\":\"%s\"}\n", jesc(r)
  else printf "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"%s\"}}\n", jesc(r)
}
') || am_unavailable
case $AM_OUT in
  ALLOW) exit 0 ;;
  '{'*) printf '%s\n' "$AM_OUT"; exit 0 ;;
esac
am_unavailable
