/**
 * 把 CLI 自己的畫面裝飾從「擷取來的即時回覆」裡刷掉。
 *
 * `turn_progress.text` 是 daemon 直接刮 tmux pane 得來的（§4.3 的同一條路），所以 agent 真的
 * 在講的話旁邊，必然混著 CLI 印給*終端使用者*看的東西：每回合的動作摘要
 * （`Read 7 files, ran 2 shell commands`）、`Tip: …`、`(ctrl+b to run in background)`、
 * `✘ Auto-update failed · Run claude doctor`、被原樣回顯的 shell 指令。這些東西進到對話氣泡裡
 * 就是實作細節外漏——使用者要看的是 agent 說了什麼，不是它的終端長什麼樣。
 *
 * 過濾一律以「行」為單位：Markdown 會把連續的單行併成同一段，所以畫面上黏在一起的雜訊
 * （`Tip: …✘ Auto-update failed`）在原始文字裡其實是分開的兩行。
 *
 * 刮得太髒（濾完什麼都不剩）就回 `null`，讓 `LiveBubble` 退回顯示 `activity` 活動摘要。
 */

/** 整行只剩框線、游標、點點：擷取到的是終端外框，不是內容。 */
const CHROME_ONLY = /^[\s>\u2500-\u257f\u2580-\u259f·•◦…‥.]*$/

/** 固定樣式的 CLI 狀態列 / 提示行。 */
const NOISE: RegExp[] = [
  // `Tip: Use /btw to ask a quick side question…`
  /^\s*(?:※\s*)?Tips?\s*[:：]/i,
  // `✘ Auto-update failed · Run claude doctor`
  /Auto-?update\s+(?:failed|available|installed)/i,
  /\brun\s+claude\s+doctor\b/i,
  // 鍵盤提示：`(ctrl+b to run in background)`、`esc to interrupt`、`? for shortcuts`
  /^\s*[(（]?\s*(?:ctrl|cmd|⌘|shift|alt|option)\s*\+/i,
  /^\s*[(（]?\s*(?:esc|escape)\b[^.]*\b(?:to|for)\b/i,
  /^\s*[(（]?\s*\?\s+for\s+shortcuts/i,
  /^\s*(?:⏵⏵|⏸)/,
  // Codex idle splash（框線裡的 banner、空輸入提示、額度重置提示）。
  /OpenAI Codex \(v/i,
  /Ask Codex to do anything/i,
  /autocompletes slash commands/i,
  /usage limit reset available/i,
  /\/model to change/i,
  /^\s*directory:\s/i,
  /^permissions:\s*YOLO/i,
  /Context \d+%\s*used/i,
  // 工具呼叫的排水溝符號：`⏺ Bash(git status)`、`⎿  Read 12 lines`
  /^\s*[⎿⏺]/,
  // spinner 行：`✻ Thinking… (12s · ↑ 1.2k tokens)`
  /^\s*[✻✽✢✳✶*+·]\s+\S+…/,
  /^\s*\S+…\s*[(（]\d+s\b/,
]

/**
 * CLI 每回合印的動作摘要行：`Ran 2 shell commands`、`Read 7 files, ran 2 shell commands`、
 * `Pushed to main, ran 1 shell command`。整行由這類子句用逗號串起來，沒有句尾標點。
 */
const SUMMARY_VERB =
  /^(?:ran|read|wrote|edited|added|removed|deleted|created|updated|searched|listed|fetched|explored|analy[sz]ed|committed|pushed|pulled|running|reading|writing|editing|searching|fetching|thinking|thought|called|did)\b/i

/**
 * 被原樣回顯的 shell 指令：`$ git status`。只認行首的 `$ `——`Running 3 shell commands… $ cd …`
 * 那種由下面的 `shell command` 規則接手。行中間的 `$ ` 不算，否則 agent 在句子裡提到
 * 「執行 $ npm run build」也會被整行吃掉。
 */
const SHELL_ECHO = /^\s*\$\s+[a-z][\w./-]*(?:[\s;|]|$)/

/**
 * CLI 的工具活動行：一句動名詞開頭的自述，接著耗時與被執行的指令片段——
 * `Moving Chrome profile out of the repo, committing · 4s "user-data-dir" foo.mjs | cut -c1-120; git rm …`
 * 判定要兩個條件同時成立（`· 4s` 這種耗時標記，加上 shell 的管線／分號／旗標），
 * 免得吃掉 agent 真的在講的、剛好帶了個秒數的句子。
 */
const TOOL_ELAPSED = /·\s*\d+(?:\.\d+)?\s*[sm]\b/
const SHELL_HINT = /[|;]|\s--?[a-z]/

/** 收尾的計時行：`Cogitated for 5m 53s · done 2:53 AM`、`Churned for 21s`、`Thought for 12s`。 */
const SPINNER_DONE = /^\s*[*✻✽✢✳✶·]?\s*[A-Za-z]+(?:ed|ing)?\s+for\s+\d+(?:\.\d+)?\s*[smh]\b/

/** `(5s · 2 lines)`：工具跑完貼在行尾的統計。 */
const TOOL_TAIL = /\([\d.]+\s*[smh]\s*·\s*\d+\s+lines?\)\s*$/

/** 貼在段落結尾的狀態字（` · summarized`），不是句子的一部分。 */
const TRAILING_STATE = /\s*·\s*(?:summari[sz]ed|compacted|truncated|cancell?ed|interrupted)\s*$/i

/** 至少要有一個字母或中日韓文字，否則這段「內容」其實什麼也沒說。 */
const HAS_WORD = /[a-z\u3400-\u9fff\u3040-\u30ff\uac00-\ud7af]/i

function isCliSummary(line: string): boolean {
  // 真正的句子會有句尾標點或中文；摘要行兩者都沒有，而且一定帶著計數的數字。
  if (/[.。！!？?：:;；]$/.test(line)) return false
  if (/[\u3400-\u9fff\u3040-\u30ff]/.test(line)) return false
  if (!/\d/.test(line) || line.length > 120) return false
  return line.split(/,\s*/).every((clause) => SUMMARY_VERB.test(clause))
}

function isNoise(line: string): boolean {
  const t = line.trim()
  if (!t) return false // 空行交給後面的段落收斂處理，不在這裡判定
  if (CHROME_ONLY.test(t)) return true
  if (SHELL_ECHO.test(t)) return true
  if (/^(?:running|ran)\b[^.]*\bshell command/i.test(t)) return true
  if (SPINNER_DONE.test(t)) return true
  if (TOOL_TAIL.test(t)) return true
  if (TOOL_ELAPSED.test(t) && SHELL_HINT.test(t)) return true
  if (/^\s*❯\s/.test(t)) return true
  if (isCliSummary(t)) return true
  return NOISE.some((re) => re.test(t))
}

// 同一份文字每次 render 都會被選取器讀一遍（群組面板還一人一份），所以留一個小快取，
// 避免對著幾 KB 的擷取文字重跑整組 regex。
const CACHE = new Map<string, string | null>()
const CACHE_MAX = 16

function scrub(raw: string): string | null {
  const kept: string[] = []
  for (const raw_line of raw.split('\n')) {
    // ` · summarized` 這種狀態字黏在句尾，整行不能丟，只能把尾巴剝掉。
    const line = raw_line.replace(TRAILING_STATE, '')
    if (isNoise(line)) continue
    // 濾掉雜訊後不要留下一串空行，把中間的空白壓成單一段落分隔。
    if (!line.trim() && (kept.length === 0 || !kept[kept.length - 1].trim())) continue
    kept.push(line)
  }
  const out = kept.join('\n').trim()
  return out && HAS_WORD.test(out) ? out : null
}

/** 濾過的即時回覆文字；整段都是 CLI 雜訊時回 `null`（氣泡改顯示活動摘要）。 */
export function cleanLiveText(raw: string | null | undefined): string | null {
  if (!raw || !raw.trim()) return null
  const hit = CACHE.get(raw)
  if (hit !== undefined) return hit
  const out = scrub(raw)
  if (CACHE.size >= CACHE_MAX) CACHE.delete(CACHE.keys().next().value as string)
  CACHE.set(raw, out)
  return out
}

/** 黏在活動摘要後面的鍵盤提示：`(ctrl+b to run in background)`、`· esc to interrupt`。 */
const ACTIVITY_HINT = /\s*[(（][^()（）]*\b(?:ctrl|cmd|⌘|shift|alt|esc|escape|tab)\b[^()（）]*[)）]/gi
const ACTIVITY_TAIL = /\s*[·•|]?\s*\b(?:esc|escape)\s+(?:to|for)\b.*$/i

/**
 * 活動摘要（`Thinking… (12s · ↑ 1.2k tokens)`）本身是有用的，但 CLI 常把「按 esc 中斷」
 * 這類只對終端有意義的提示接在同一行後面。留下狀態，丟掉按鍵教學。
 */
export function cleanLiveActivity(raw: string | null | undefined): string | null {
  if (!raw) return null
  const out = raw.replace(ACTIVITY_HINT, '').replace(ACTIVITY_TAIL, '').trim()
  return out && HAS_WORD.test(out) ? out : null
}
