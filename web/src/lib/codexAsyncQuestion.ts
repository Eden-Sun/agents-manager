/**
 * codex 的非同步提問（`request_user_input_async`，issue #1879）在畫面上的樣子。
 *
 * 這個工具不擋回合：題目只排在 TUI 的 `Queued follow-up inputs`，收成一行
 * `? 1 question · 7s / shift+← to answer`；題目與選項畫面上看不到（daemon 從 rollout 讀，`pending-question`）。
 * 按 shift+← 展開成編號選單（`parseChoiceMenu` 認得，`menu.asyncQueue`）或自由輸入框（`Type your answer`）。
 * 同 daemon `codex_async_question::async_screen`（codex 0.163.0 真機畫面）；作答前每一步都用它重讀畫面對帳。
 */

export interface AsyncScreen {
  /** 收合：`? N question(s)` ＋ `shift+← to answer`；展開：題目在畫面上，底下 `enter submit  ^] skip`。 */
  expanded: boolean
  /** 收合時是畫面寫的題數；展開時是 `k of n` 的 n，沒有那行就是 1。 */
  count: number
}

/** 只看畫面底下這麼多行：上面的對話紀錄會原樣引用題目文字。 */
const SCAN = 40

const COLLAPSED = /^\?\s+(\d+)\s+questions?(?:\s.*)?$/
/** Both platform renderings describe the same queue; these glyphs are never outgoing keys. */
export const ASYNC_QUESTION_FOOTER = /^\s*enter submit\b.*(?:\^|⌃)\]\s*skip/i
const PAGE_OF = /^\d+ of (\d+)$/

/** 畫面上有沒有非同步提問、什麼狀態；沒有回 `null`。 */
export function parseAsyncScreen(text: string | null | undefined): AsyncScreen | null {
  if (!text) return null
  const all = text.split('\n').map((l) => l.trim())
  const lines = all.slice(Math.max(0, all.length - SCAN))
  for (let i = lines.length - 1; i >= 0; i--) {
    const collapsed = COLLAPSED.exec(lines[i])
    if (collapsed && Number(collapsed[1]) > 0) {
      const next = lines.slice(i + 1).find((l) => l !== '')
      if (next?.startsWith('shift+← to answer') || next?.startsWith('⇧← to answer')) return { expanded: false, count: Number(collapsed[1]) }
    }
    if (ASYNC_QUESTION_FOOTER.test(lines[i])) {
      const page = lines
        .slice(Math.max(0, i - 8), i)
        .reverse()
        .map((l) => PAGE_OF.exec(l))
        .find(Boolean)
      return { expanded: true, count: page ? Number(page[1]) : 1 }
    }
  }
  return null
}

const QUEUE_HEADER = 'Queued follow-up inputs'

const squash = (s: string) => s.replace(/\s+/g, '')

/** 排隊面板本身：最後一個 `Queued follow-up inputs` 之下、腳註之上；上面的對話紀錄會原樣引用題目，不算。沒有面板回 `null`。 */
export function asyncPanelText(text: string | null | undefined): string | null {
  if (!text) return null
  const lines = text.split('\n')
  let head = -1
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].trim().replace(/^•\s*/, '') === QUEUE_HEADER) {
      head = i
      break
    }
  }
  if (head < 0) return null
  const tail = lines.slice(head + 1)
  const end = tail.findIndex((l) => l.trim().startsWith('enter submit'))
  return (end < 0 ? tail : tail.slice(0, end)).join('\n')
}

/** 面板的非空行（去掉多題時的 `1 of 2` 頁碼行）。 */
function panelLines(text: string | null | undefined): string[] | null {
  const panel = asyncPanelText(text)
  if (panel === null) return null
  const lines = panel.split('\n').map((l) => l.trim()).filter((l) => l !== '')
  if (lines.length && PAGE_OF.test(lines[0])) lines.shift()
  return lines
}

/**
 * 展開中的面板：開頭的題目行**完整等於**預期的原文（折行與空白不計）時，回題目之後的內容（選項列或輸入框，逐行）；
 * 不是展開中、沒有面板、題目對不上都回 `null`。完整比對：舊題「Name?」不會對上新題「Full Name?」，反之亦然
 * （子字串比對會；issue #1879 複審）。
 */
export function panelBody(text: string | null | undefined, question: string): string[] | null {
  if (parseAsyncScreen(text)?.expanded !== true) return null
  const lines = panelLines(text)
  const want = squash(question)
  if (!lines || !want) return null
  let acc = ''
  for (let k = 0; k < lines.length; k++) {
    acc += squash(lines[k])
    if (acc === want) return lines.slice(k + 1)
    if (acc.length >= want.length) return null
  }
  return null
}

/** 提問面板展開中、而且題目就是這一題。 */
export function panelShows(text: string | null | undefined, question: string): boolean {
  return panelBody(text, question) !== null
}

/** 自由輸入題輸入框的完整內容（空白不計）：空的時候是 `Type your answer` 佔位字。面板或題目對不上回 `null`。 */
export function panelInput(text: string | null | undefined, question: string): string | null {
  const body = panelBody(text, question)
  return body === null ? null : squash(body.join(''))
}

/** 空輸入框（佔位字）的 `panelInput`。 */
export const EMPTY_INPUT = squash('Type your answer')

/** 輸入框內容是否完整等於 `typed`（不是只比後綴：原本有 Alice 再貼 Bob 是 AliceBob）。 */
export const inputEquals = (input: string | null, typed: string): boolean => input !== null && input === squash(typed)
