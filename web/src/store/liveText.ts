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
  // 鍵盤提示：`(ctrl+b to run in background)`、`esc to interrupt`、`? for shortcuts`。
  // esc 提示的形狀是 `esc to <動作>` 後面接行尾或 `·`／括號；`Escape the string to avoid …` 是正文（#1689）。
  /^\s*[(（]?\s*(?:ctrl|cmd|⌘|shift|alt|option)\s*\+/i,
  /^\s*[(（]?\s*(?:esc|escape)\s+(?:\w+\s+)?(?:to|for)\s+\w+\s*[)）]?\s*(?:[·•|]|$)/i,
  // footer 只認行尾或右欄（與正文之間至少兩格空白）；`? for shortcuts explains the UI` 是正文（同 daemon #1495）。
  /^\s*[(（]?\s*\?\s+for\s+shortcuts\s*[)）]?\s*$/i,
  /^\s*[(（]?\s*\?\s+for\s+shortcuts\s{2,}\S/i,
  // Codex footer: warning counter may have its own row in a narrow pane.
  /^\s*⚠ \d+ warnings? · f2 to view\s*$/,
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
  // 點字字頭（`⠀-⣿`）是舊版 spinner（`⠦ Thinking… 52s`，同 daemon is_spinner_glyph）。
  /^\s*[✻✽✢✳✶*+·⠀-⣿]\s+\S+…/,
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

/**
 * `Cogitated for 5m 53s · done 2:53 AM`、`Thought for 12s`。只認過去式或進行式的動詞（`-ed`／`-ing`／`Thought`），
 * 而且時間之後只能是行尾或括號／`·` 的註記：`Wait for 30s before retrying…` 是散文，不是 spinner。
 */
const SPINNER_DONE = /^\s*[*✻✽✢✳✶·]?\s*(?:[A-Za-z]+(?:ed|ing)|Thought)\s+for\s+\d+(?:\.\d+)?\s*[smh](?:\s+\d+\s*[sm])?(?:\s*[·(（].*)?\s*$/

/** `(5s · 2 lines)` */
const TOOL_TAIL = /\([\d.]+\s*[smh]\s*·\s*\d+\s+lines?\)\s*$/

/** ` · summarized` */
const TRAILING_STATE = /\s*·\s*(?:summari[sz]ed|compacted|truncated|cancell?ed|interrupted)\s*$/i

const HAS_WORD = /\p{L}/u

function isCliSummary(line: string): boolean {
  // 摘要行沒有句尾標點與中文，且一定帶數字。
  if (/[.。！!？?：:;；]$/.test(line)) return false
  if (/[\u3400-\u9fff\u3040-\u30ff]/.test(line)) return false
  if (!/\d/.test(line) || line.length > 120) return false
  return line.split(/,\s*/).every((clause) => SUMMARY_VERB.test(clause))
}

/**
 * claude 權限模式列（同 daemon `claude_mode::is_mode_row`，#788）：`⏵⏵` 開頭一律算；`⏸` 只在第一段（` · ` 之前）
 * 是模式名、或整段以 `(shift+tab to cycle)` 結尾時才算。`⏸ 暫停部署` 這種正文不是模式列。
 */
function isModeRow(t: string): boolean {
  if (t.startsWith('⏵⏵')) return true
  const low = t.toLowerCase()
  const rest = low.startsWith('⏸') ? low.slice('⏸'.length) : low
  const seg = (rest.split(' · ')[0] ?? '').trim()
  return seg.endsWith('(shift+tab to cycle)') || ['manual mode on', 'plan mode on', 'accept edits on', 'bypass permissions on'].includes(seg)
}

/**
 * codex 狀態列 `模型 [effort…] · ~/cwd …`（同 daemon `is_codex_status_row`，#1555）：模型名要含 `-` 且以英數字開頭，
 * 所以 markdown 項目 `- 設定檔 · ~/…` 不是狀態列；effort 不限於清單（`minimal`、`none`…）。
 */
function isCodexStatusRow(t: string): boolean {
  const i = t.indexOf('·')
  if (i < 0) return false
  const model = (t.slice(0, i).trim().split(/\s+/)[0] ?? '')
  if (!model.includes('-') || !/^[a-z0-9]/i.test(model)) return false
  const rest = t.slice(i + 1).trim()
  return rest.startsWith('/') || rest.startsWith('~') || rest.includes('Context') || rest.includes('% left')
}

function isNoise(line: string): boolean {
  const t = line.trim()
  if (!t) return false // 空行由 scrub 收斂
  if (CHROME_ONLY.test(t)) return true
  if (isModeRow(t) || isCodexStatusRow(t)) return true
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
