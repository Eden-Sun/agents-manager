/**
 * 從刮 pane 得來的 `turn_progress.text`（§4.3）刷掉 CLI 的終端裝飾（動作摘要、Tip、按鍵提示…）。
 * 以行為單位過濾（Markdown 會把單行黏成一段，原文其實分行）；濾完不剩就回 null，氣泡改顯示活動摘要。
 */

/** 整行只剩框線、游標、點點。 */
const CHROME_ONLY = /^[\s>\u2500-\u257f\u2580-\u259f·•◦…‥.]*$/

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

/** 動作摘要行的子句：`Read 7 files, ran 2 shell commands`（逗號串接、無句尾標點）。 */
const SUMMARY_VERB =
  /^(?:ran|read|wrote|edited|added|removed|deleted|created|updated|searched|listed|fetched|explored|analy[sz]ed|committed|pushed|pulled|running|reading|writing|editing|searching|fetching|thinking|thought|called|did)\b/i

/** 行首 `$ git status` 回顯；行中間的 `$ ` 不算，免得吃掉「執行 $ npm run build」這種句子。 */
const SHELL_ECHO = /^\s*\$\s+[a-z][\w./-]*(?:[\s;|]|$)/

/**
 * 工具活動行（`Moving …, committing · 4s "x" foo.mjs | cut …`）：耗時標記與 shell 管線／旗標
 * 要同時成立，免得吃掉剛好帶秒數的真句子。
 */
const TOOL_ELAPSED = /·\s*\d+(?:\.\d+)?\s*[sm]\b/
const SHELL_HINT = /[|;]|\s--?[a-z]/

/** `Cogitated for 5m 53s · done 2:53 AM`、`Thought for 12s`。 */
const SPINNER_DONE = /^\s*[*✻✽✢✳✶·]?\s*[A-Za-z]+(?:ed|ing)?\s+for\s+\d+(?:\.\d+)?\s*[smh]\b/

/** `(5s · 2 lines)` */
const TOOL_TAIL = /\([\d.]+\s*[smh]\s*·\s*\d+\s+lines?\)\s*$/

/** ` · summarized` */
const TRAILING_STATE = /\s*·\s*(?:summari[sz]ed|compacted|truncated|cancell?ed|interrupted)\s*$/i

const HAS_WORD = /[a-z\u3400-\u9fff\u3040-\u30ff\uac00-\ud7af]/i

function isCliSummary(line: string): boolean {
  // 摘要行沒有句尾標點與中文，且一定帶數字。
  if (/[.。！!？?：:;；]$/.test(line)) return false
  if (/[\u3400-\u9fff\u3040-\u30ff]/.test(line)) return false
  if (!/\d/.test(line) || line.length > 120) return false
  return line.split(/,\s*/).every((clause) => SUMMARY_VERB.test(clause))
}

function isNoise(line: string): boolean {
  const t = line.trim()
  if (!t) return false // 空行由 scrub 收斂
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

// 選取器每次 render 都讀（群組面板一人一份），快取避免對幾 KB 文字重跑整組 regex。
const CACHE = new Map<string, string | null>()
const CACHE_MAX = 16

function scrub(raw: string): string | null {
  const kept: string[] = []
  for (const raw_line of raw.split('\n')) {
    const line = raw_line.replace(TRAILING_STATE, '')
    if (isNoise(line)) continue
    // 連續空行壓成一個段落分隔。
    if (!line.trim() && (kept.length === 0 || !kept[kept.length - 1].trim())) continue
    kept.push(line)
  }
  const out = kept.join('\n').trim()
  return out && HAS_WORD.test(out) ? out : null
}

export function cleanLiveText(raw: string | null | undefined): string | null {
  if (!raw || !raw.trim()) return null
  const hit = CACHE.get(raw)
  if (hit !== undefined) return hit
  const out = scrub(raw)
  if (CACHE.size >= CACHE_MAX) CACHE.delete(CACHE.keys().next().value as string)
  CACHE.set(raw, out)
  return out
}

const ACTIVITY_HINT = /\s*[(（][^()（）]*\b(?:ctrl|cmd|⌘|shift|alt|esc|escape|tab)\b[^()（）]*[)）]/gi
const ACTIVITY_TAIL = /\s*[·•|]?\s*\b(?:esc|escape)\s+(?:to|for)\b.*$/i

/** 活動摘要留下狀態，丟掉黏在後面的按鍵提示（`esc to interrupt`）。 */
export function cleanLiveActivity(raw: string | null | undefined): string | null {
  if (!raw) return null
  const out = raw.replace(ACTIVITY_HINT, '').replace(ACTIVITY_TAIL, '').trim()
  return out && HAS_WORD.test(out) ? out : null
}
