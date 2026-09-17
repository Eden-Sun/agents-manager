/**
 * 從終端快照認出 agent 停著的編號選單，讓 web 畫成可點清單
 * （2026-09-12 使用者：手機選第 4 項要按三次 ↓，選項文字還被截掉）。
 *
 * 形狀：claude 2.x AskUserQuestion 單選／多分頁＋多選（2026-09-12 第三輪），也吃權限框
 * `│ ❯ 1. Yes │` 與 codex `> 1. Update now`。多選頁的坑：說明只縮排 2（跟編號同欄）、
 * 有沒編號但游標走得到的 `Submit` 列、選項間夾分隔線。
 *
 * 寧可不認也不要認錯（按鍵直送別人終端）：≥2 個編號、從 1 連號、剛好一個游標記號、
 * 整塊在畫面底（TAIL_LIMIT），缺一回 `null`，UI 退回按鍵面板。
 */

export interface TuiChoice {
  /** 畫面上印的編號；送鍵時不靠它。 */
  number: number
  /** 去掉游標記號、編號與 `[ ]` 的本文。 */
  title: string
  /** 說明（折行已接回）；沒有是空字串。 */
  detail: string
  current: boolean
  /** 多選的勾選狀態；單選一律 `null`。 */
  checked: boolean | null
}

/** 分頁列的一格（問卷裡的一題）。 */
export interface TuiTab {
  label: string
  /** `☒`＝答過。 */
  done: boolean
  /** `✔ Submit` 送出頁，不是一題。 */
  submit: boolean
}

export interface TuiChoiceMenu {
  /** 折行已接回；認不出問句是 `null`。 */
  question: string | null
  choices: TuiChoice[]
  /** 游標停在沒編號的 `Submit` 列時是 `-1`。 */
  cursor: number
  /** 有 claude 腳註；只是信心指標。 */
  footer: boolean
  /** 帶 `[ ]`：點一下是切換勾選，不是送出。 */
  multi: boolean
  /** 不是多題問卷就是空陣列。 */
  tabs: TuiTab[]
  /** 猜的目前分頁（第一個未答，全答完＝Submit 格）；終端用顏色標、快照讀不到，UI 要照實說是猜的。 */
  tabAt: number | null
  /** review／confirm 頁的「● 問題 / → 答案」；一般頁是空陣列。 */
  review: { question: string; answer: string }[]
  /** 沒編號的 `Submit` 列（多選才有）；`after` 是排在第幾個選項後面。 */
  submit: { after: number; current: boolean } | null
}

/** 只看畫面底下這麼多行，正文裡的編號清單不算。 */
const SCAN_LINES = 60

/** 最後一個選項到畫面底之間最多幾行有字的東西（腳註、輸入框、狀態列）。 */
const TAIL_LIMIT = 10

/** 兩個編號間最多夾幾行說明；再多就是別的東西混進來。 */
const DETAIL_LIMIT = 8

const ITEM = /^(\s*)([❯›▶>]\s)?(\d{1,2})[.)](\s+)(\S.*)$/

const CHECKBOX = /^\[([ xX*✓✔])\]\s*(.*)$/

const SUBMIT = /^\s*([❯›▶>]\s*)?Submit\s*$/

/** 有它幾乎確定是選單，但沒有也可能是（權限框就沒有）。 */
const FOOTER = /enter to select|to navigate|esc to cancel/i

/** claude 工具呼叫的行首符號：往上找問題撞到就停。 */
const TRANSCRIPT = /^[⏺⎿✽·✢✻✶*]/

const TAB_MARK = /([☒☑☐✔✓])\s*([^☒☑☐✔✓←→]+)/g

/**
 * 分頁列往上找幾行：review 頁中間隔著「每題 → 答案」，只看問題正上方會找不到
 * （2026-09-12 第七輪：最後一頁點不到分頁）。取最靠近的，遇分隔線／正文就停。
 */
const TAB_SCAN = 20

const REVIEW_Q = /^\s*[●•]\s+(\S.*)$/
const REVIEW_A = /^\s*(?:→|->)\s*(\S.*)$/

/** 去掉權限框框線、保留框內縮排。只吃真框線字元，ASCII `|` 太常出現在正文。 */
function stripBox(line: string): string {
  const s = line.replace(/\s+$/, '')
  const lead = /^\s*[│┃]\s?/.exec(s)
  if (!lead) return s
  return s.slice(lead[0].length).replace(/\s*[│┃]$/, '')
}

/** 選單中間會夾分隔線，不能當成結束。 */
function isDivider(s: string): boolean {
  return s.trim() !== '' && /^[\s─-╿]+$/.test(s)
}

function indentOf(s: string): number {
  return s.length - s.trimStart().length
}

function hasMarker(s: string): boolean {
  return /^\s*[❯›▶>]\s/.test(s)
}

/** 接回終端折行：CJK 之間不補空白，其餘補一個；只求通順，不保證還原原文。 */
function joinWrapped(a: string, b: string): string {
  if (!a) return b
  if (!b) return a
  const cjk = /[⺀-鿿　-〿＀-￯]/
  return cjk.test(a[a.length - 1]) || cjk.test(b[0]) ? a + b : `${a} ${b}`
}

interface Row {
  line: number
  marker: boolean
  number: number
  title: string
  checked: boolean | null
}

function matchRow(line: number, s: string): Row | null {
  const m = ITEM.exec(s)
  if (!m) return null
  const [, , marker, num, , rest] = m
  const box = CHECKBOX.exec(rest.trim())
  return {
    line,
    marker: Boolean(marker),
    number: Number(num),
    title: (box ? box[2] : rest).trim(),
    checked: box ? box[1] !== ' ' : null,
  }
}

/** 分頁列（`←  ☒ 編輯方式  ☐ 功能  ✔ Submit  →`）。目前題是顏色標的、讀不到，只回答過沒有。 */
function parseTabs(s: string): TuiTab[] | null {
  if (!/[☒☑☐✔✓]/.test(s)) return null
  const out: TuiTab[] = []
  TAB_MARK.lastIndex = 0
  for (let m = TAB_MARK.exec(s); m; m = TAB_MARK.exec(s)) {
    const label = m[2].trim()
    if (!label) continue
    out.push({ label, done: m[1] === '☒' || m[1] === '☑', submit: /^submit$/i.test(label) })
  }
  return out.length >= 2 ? out : null
}

/** 認不出來回 `null`，呼叫端畫按鍵面板，不要猜。 */
export function parseChoiceMenu(text: string | null | undefined): TuiChoiceMenu | null {
  if (!text) return null
  const all = text.split('\n').map(stripBox)
  const lines = all.slice(Math.max(0, all.length - SCAN_LINES))

  // 1. 由下往上找最後一個選項行。
  let end = -1
  let footer = false
  let tail = 0
  for (let i = lines.length - 1; i >= 0; i--) {
    if (matchRow(i, lines[i])) {
      end = i
      break
    }
    if (FOOTER.test(lines[i])) footer = true
    if (lines[i].trim() === '' || isDivider(lines[i])) continue
    if (++tail > TAIL_LIMIT) return null
  }
  if (end < 0) return null

  // 2. 往上收到編號 1。編號間的非編號行都當說明（縮排靠不住：單選縮 5、多選縮 2），
  // 只挑出沒編號的 `Submit` 列。
  const rows: Row[] = []
  let submitLine = -1
  let gap = 0
  for (let i = end; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (row) {
      // 跳號、重號＝不是同一份選單。
      if (rows.length && row.number !== rows[rows.length - 1].number - 1) break
      rows.push(row)
      gap = 0
      if (row.number === 1) break
      continue
    }
    if (lines[i].trim() === '' || isDivider(lines[i])) continue
    if (SUBMIT.test(lines[i])) {
      submitLine = i
      continue
    }
    if (!rows.length || ++gap > DETAIL_LIMIT) break
  }
  rows.reverse()

  if (rows.length < 2 || rows[0].number !== 1) return null

  // 沒有 `Chat about this` 時 `Submit` 排在最後一個編號之後，往下補看幾行。
  if (submitLine < 0) {
    let blanks = 0
    for (let i = end + 1; i < lines.length && i <= end + 4; i++) {
      if (FOOTER.test(lines[i])) break
      if (lines[i].trim() === '') {
        if (++blanks > 1) break
        continue
      }
      if (SUBMIT.test(lines[i])) {
        submitLine = i
        break
      }
      if (!isDivider(lines[i])) break
    }
  }

  // 游標記號剛好一個：零個＝捲出畫面算不出步數，多個＝正文引用之類不是選單。
  const blockTop = rows[0].line
  const blockEnd = Math.max(end, submitLine)
  let markers = 0
  for (let i = blockTop; i <= blockEnd; i++) if (hasMarker(lines[i])) markers++
  if (markers !== 1) return null

  // 3. 說明接成一段；最後一項收到空白行／分隔線為止。
  const choices: TuiChoice[] = rows.map((row, idx) => {
    const stop = idx + 1 < rows.length ? rows[idx + 1].line : Math.min(lines.length, row.line + 1 + TAIL_LIMIT)
    let detail = ''
    for (let i = row.line + 1; i < stop; i++) {
      const s = lines[i]
      if (i === submitLine) continue
      if (s.trim() === '' || isDivider(s)) {
        // 選項之間的空白／分隔線不算結束（真機 5、6 之間就有）。
        if (idx + 1 < rows.length) continue
        break
      }
      if (idx + 1 === rows.length && indentOf(s) < 2) break
      detail = joinWrapped(detail, s.trim())
    }
    return { number: row.number, title: row.title, detail, current: row.marker, checked: row.checked }
  })

  // 4. 問題。
  let q = rows[0].line - 1
  while (q >= 0 && lines[q].trim() === '') q--
  const qs: string[] = []
  while (q >= 0 && qs.length < 4) {
    const s = lines[q]
    if (s.trim() === '' || isDivider(s) || TRANSCRIPT.test(s.trim()) || matchRow(q, s)) break
    if (parseTabs(s) || REVIEW_Q.test(s) || REVIEW_A.test(s)) break
    qs.unshift(s.trim())
    q--
  }

  // 5. 分頁列（見 TAB_SCAN）。
  let tabLine = -1
  for (let i = rows[0].line - 1; i >= 0 && rows[0].line - i <= TAB_SCAN; i--) {
    if (isDivider(lines[i]) || TRANSCRIPT.test(lines[i].trim())) break
    if (parseTabs(lines[i])) {
      tabLine = i
      break
    }
  }
  const tabs = (tabLine >= 0 ? parseTabs(lines[tabLine]) : null) ?? []

  // 6. review 段（review 頁才有）。
  const review: { question: string; answer: string }[] = []
  for (let i = tabLine + 1; tabLine >= 0 && i < rows[0].line; i++) {
    const mq = REVIEW_Q.exec(lines[i])
    if (mq) {
      review.push({ question: mq[1].trim(), answer: '' })
      continue
    }
    const ma = REVIEW_A.exec(lines[i])
    if (ma && review.length) {
      const last = review[review.length - 1]
      last.answer = joinWrapped(last.answer, ma[1].trim())
      continue
    }
    // Type something 換行貼上之後，review 第二行沒有 `→`（2026-09-13 真機）。
    if (review.length && indentOf(lines[i]) >= 3 && lines[i].trim() && !matchRow(i, lines[i]) && !REVIEW_Q.test(lines[i])) {
      const last = review[review.length - 1]
      last.answer = joinWrapped(last.answer, lines[i].trim())
    }
  }

  const undone = tabs.findIndex((t) => !t.done && !t.submit)
  const at = undone >= 0 ? undone : tabs.findIndex((t) => t.submit)

  const submitAfter = submitLine < 0 ? -1 : rows.filter((r) => r.line < submitLine).length - 1

  return {
    question: qs.length ? qs.reduce((a, b) => joinWrapped(a, b), '') : null,
    choices,
    cursor: rows.findIndex((r) => r.marker),
    footer,
    multi: choices.some((c) => c.checked !== null),
    tabs,
    tabAt: at >= 0 ? at : null,
    review,
    submit: submitLine < 0 ? null : { after: submitAfter, current: hasMarker(lines[submitLine]) },
  }
}

export type WalkTarget = number | 'submit'

/** 游標走得到的列照畫面排序；`Submit` 夾在 5、6 之間要算進 ↓ 次數。認不得的列靠 `walk` 重讀補正。 */
function walkRows(menu: TuiChoiceMenu): WalkTarget[] {
  const rows: WalkTarget[] = menu.choices.map((_, i) => i)
  if (menu.submit) rows.splice(menu.submit.after + 1, 0, 'submit')
  return rows
}

/**
 * 走到 `target` 的方向鍵（不含 Enter）。用 ↑↓ 不用數字：每種選單都吃、超過 9 項也行，數字鍵
 * 是否直接選取各家不一（見 `daemon/src/tui_prompts.rs`）。算不出來回 `null`，呼叫端別送。
 */
export function keysToMove(menu: TuiChoiceMenu, target: WalkTarget): string[] | null {
  const rows = walkRows(menu)
  const from = menu.submit?.current ? rows.indexOf('submit') : rows.indexOf(menu.cursor)
  const to = rows.indexOf(target)
  if (from < 0 || to < 0) return null
  const delta = to - from
  return Array.from({ length: Math.abs(delta) }, () => (delta > 0 ? 'down' : 'up'))
}

/** 單選：走過去再 Enter，一批送完（無 Submit 列、步數算得準；2026-09-12 第一輪實測）。多選走一步驗一步，見 `BlockedChoices`。 */
export function keysToSelect(menu: TuiChoiceMenu, target: number): string[] {
  return [...(keysToMove(menu, target) ?? []), 'enter']
}

/** `Type something.` / `Chat about this`：編號列，但不是勾選項。 */
export function isActionChoice(c: Pick<TuiChoice, 'title'>): boolean {
  return isTypeSomething(c) || /^chat about this$/i.test(c.title.trim())
}

/** 游標停在這列時可以直接貼字，標題會被取代（2026-09-13 真機）。 */
export function isTypeSomething(c: Pick<TuiChoice, 'title'>): boolean {
  return /^type something\.?$/i.test(c.title.trim())
}

/**
 * 這一列能不能在面板裡打字作答。只有**單選頁**真機驗過（走到 → 貼字 → 對帳 → Enter）；複選頁的
 * `[ ] Type something` 怎麼輸入沒驗過，而且在複選頁按 Enter 可能直接交卷——以前面板照樣給打字框，
 * 送出時只勾了空白的那一列、字完全沒送（review3 c1 M8）。複選頁的自訂文字請到終端打。
 */
export function typedAnswerHere(menu: { multi: boolean }, c: Pick<TuiChoice, 'title'>): boolean {
  return isTypeSomething(c) && !menu.multi
}

/** 複選頁的 `Type something` 在面板裡勾不起來（勾了也送不出字），要到終端打。 */
export const MULTI_TYPE_HINT = '複選題的「Type something」要在終端打字（按「展開全畫面」或「終端原文與更多按鍵」），這裡勾了也送不出文字。'

/** 貼上後那列是否已長出這段字：換行貼上時第一行在標題、其餘在說明；答完會多 `✔`。 */
export function customAnswerShown(choice: TuiChoice, text: string): boolean {
  // 空白全部拿掉再比：窄 pane 會把長答案硬折行（中文沒有空格，折在字中間），折出來的斷點在
  // 標題與說明之間變成一個空格，照原樣比就永遠對不上——會判成「沒出現」而不按 Enter，重送又變成
  // 「選項跟讀進來時不一樣」（review3 c1 f19e855 的疑點）。英文折在空格上，去掉空白照樣對得上。
  const squash = (s: string) => s.replace(/\s+/g, '')
  const want = squash(text)
  if (!want) return false
  const title = choice.title.replace(/\s*✔\s*$/, '')
  const blob = squash(`${title} ${choice.detail ?? ''}`)
  const first = squash(text.split('\n')[0] ?? '')
  return blob.includes(want) || (first !== '' && blob.includes(first))
}

/** 送鍵前確認選單還是同一份（↓ 次數是相對的）。不比勾選與游標：那是我們自己按出來的。 */
export function sameChoices(a: TuiChoiceMenu, b: TuiChoiceMenu): boolean {
  return (
    a.question === b.question &&
    a.choices.length === b.choices.length &&
    a.choices.every((c, i) => c.number === b.choices[i].number && c.title === b.choices[i].title)
  )
}
