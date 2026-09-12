/**
 * 從終端快照裡認出 agent 停在上面的「編號選單」，好讓 web 把它畫成可以點的清單
 * （2026-09-12 使用者：手機上要選第 4 項得按三次 ↓ 再 Enter，而且選項文字被終端寬度截掉）。
 *
 * 認的是這種畫面（claude 2.x 的 AskUserQuestion，真機 185 欄快照）：
 *
 * ```text
 * robinstech-carbis-web 要先推進哪一塊？
 *
 * ❯ 1. 先把 WIP commit 掉（建議）
 *      57 個檔案未 commit，包含 #21 的評價 demo…
 *      feat/astro-migration。
 *   2. 接 console 的回饋 API（#22）
 *      把 api/feedback.ts 改成轉送 console…
 *   5. Type something.
 * ────────────────────────────────────────────
 *   6. Chat about this
 *
 * Enter to select · ↑/↓ to navigate · Esc to cancel
 * ```
 *
 * 同一套也吃 claude 的權限框（`│ ❯ 1. Yes │`）與 codex 的 `> 1. Update now`。
 *
 * **寧可不認也不要認錯**：按鍵是直接送進別人終端的，認錯的代價是替使用者答了一題。所以
 * 四個條件全中才算數——至少兩個編號、編號從 1 連號、**剛好一個**游標記號（`❯`／`>`）、
 * 而且整塊就在畫面底下（[`TAIL_LIMIT`]）。少一個就回 `null`，UI 照舊退回按鍵面板。
 */

export interface TuiChoice {
  /** 畫面上印的編號（`1.` 的 1）；跟陣列索引一致但意義不同，送鍵時不靠它。 */
  number: number
  /** 第一行的選項本文（去掉游標記號與編號）。 */
  title: string
  /** 底下縮排的說明，終端折行的地方已經接回一段。沒有就是空字串。 */
  detail: string
  /** 游標現在停在這一項。 */
  current: boolean
}

export interface TuiChoiceMenu {
  /** 選單上方那句問題（折行已接回）；上面沒有可辨識的問句就是 `null`。 */
  question: string | null
  choices: TuiChoice[]
  /** 游標停在 `choices` 的第幾個。 */
  cursor: number
  /** 畫面上有 claude 的腳註（`Enter to select · ↑/↓ to navigate`）。只是信心指標。 */
  footer: boolean
}

/** 只看畫面底下這麼多行：選單畫在輸入列的位置，正文裡的編號清單不算。 */
const SCAN_LINES = 60

/** 最後一個選項到畫面底之間，最多容得下這麼多行有字的東西（腳註、輸入框、狀態列）。 */
const TAIL_LIMIT = 10

/** 一行選項：`❯ 1. 標題` / `  2. 標題` / `> 1. 標題`。 */
const ITEM = /^(\s*)([❯›▶>]\s)?(\d{1,2})[.)](\s+)(\S.*)$/

/** claude 的腳註。有它就幾乎確定是選單，但沒有也還是可能是（權限框就沒有）。 */
const FOOTER = /enter to select|to navigate|esc to cancel/i

/** claude 印工具呼叫與結果的行首符號：問題往上找時撞到它就停，那已經是正文了。 */
const TRANSCRIPT = /^[⏺⎿✽·✢✻✶*]/

/**
 * 去掉外框線：claude 的權限框是 `│ ❯ 1. Yes                    │`，框內的縮排要原樣留著
 * （說明行就是靠縮排認出來的）。只吃真正的框線字元，ASCII 的 `|` 太常出現在正文裡。
 */
function stripBox(line: string): string {
  const s = line.replace(/\s+$/, '')
  const lead = /^\s*[│┃]\s?/.exec(s)
  if (!lead) return s
  return s.slice(lead[0].length).replace(/\s*[│┃]$/, '')
}

/** 整行都是框線／分隔線（`─────`、`╭───╮`）。選單中間會夾這種行，不能當成結束。 */
function isDivider(s: string): boolean {
  return s.trim() !== '' && /^[\s─-╿]+$/.test(s)
}

function indentOf(s: string): number {
  return s.length - s.trimStart().length
}

/**
 * 接回被終端折掉的一行。
 *
 * 中日韓字元之間不補空白（補了會在句子中間多一個洞），其餘照英文的規矩補一個。
 * 終端折行本來就可能折在單字中間，這裡只求讀得通順，不保證還原原文。
 */
function joinWrapped(a: string, b: string): string {
  if (!a) return b
  if (!b) return a
  const cjk = /[⺀-鿿　-〿＀-￯]/
  return cjk.test(a[a.length - 1]) || cjk.test(b[0]) ? a + b : `${a} ${b}`
}

interface Row {
  line: number
  /** 標題起始的欄位；說明行的縮排要對得上這裡才算它的說明。 */
  textCol: number
  marker: boolean
  number: number
  title: string
}

function matchRow(line: number, s: string): Row | null {
  const m = ITEM.exec(s)
  if (!m) return null
  const [, indent, marker, num, gap, title] = m
  return {
    line,
    textCol: indent.length + (marker ? marker.length : 0) + num.length + 1 + gap.length,
    marker: Boolean(marker),
    number: Number(num),
    title: title.trim(),
  }
}

/** 認不出來就回 `null`——呼叫端照舊畫按鍵面板，不要猜。 */
export function parseChoiceMenu(text: string | null | undefined): TuiChoiceMenu | null {
  if (!text) return null
  const all = text.split('\n').map(stripBox)
  const lines = all.slice(Math.max(0, all.length - SCAN_LINES))

  // 1. 由下往上找最後一個選項行。中間只容得下 TAIL_LIMIT 行有字的東西（腳註／輸入列）。
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

  // 2. 繼續往上收，收到編號 1 為止。夾在中間的只能是空白行、分隔線，或縮排夠深的說明行。
  const rows: Row[] = []
  for (let i = end; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (row) {
      // 往上走編號要遞減：跳號、重號都當成「這不是同一份選單」。
      if (rows.length && row.number !== rows[rows.length - 1].number - 1) break
      rows.push(row)
      if (row.number === 1) break
      continue
    }
    if (lines[i].trim() === '' || isDivider(lines[i])) continue
    if (rows.length && indentOf(lines[i]) >= rows[rows.length - 1].textCol - 1) continue
    break
  }
  rows.reverse()

  if (rows.length < 2 || rows[0].number !== 1) return null
  const markers = rows.filter((r) => r.marker)
  // 剛好一個游標記號：零個代表游標捲出畫面（算不出 ↓ 幾次），兩個以上代表這根本不是選單
  // （例如正文裡的 `> 1. …` 引用）。
  if (markers.length !== 1) return null

  // 3. 每一項底下縮排的說明行接成一段。最後一項往下收到空白行／分隔線為止。
  const choices: TuiChoice[] = rows.map((row, idx) => {
    const stop = idx + 1 < rows.length ? rows[idx + 1].line : Math.min(lines.length, row.line + 1 + TAIL_LIMIT)
    let detail = ''
    for (let i = row.line + 1; i < stop; i++) {
      const s = lines[i]
      if (s.trim() === '' || isDivider(s)) {
        // 選項之間夾的空白／分隔線不算結束（真機快照在 5 與 6 之間就有一條）；
        // 最後一項後面的空白行則是選單的下緣。
        if (idx + 1 < rows.length) continue
        break
      }
      if (indentOf(s) < row.textCol - 1) break
      detail = joinWrapped(detail, s.trim())
    }
    return { number: row.number, title: row.title, detail, current: row.marker }
  })

  // 4. 第一項上面那句問題（先跳過空白行，再往上收連續的幾行）。
  let q = rows[0].line - 1
  while (q >= 0 && lines[q].trim() === '') q--
  const qs: string[] = []
  while (q >= 0 && qs.length < 4) {
    const s = lines[q]
    if (s.trim() === '' || isDivider(s) || TRANSCRIPT.test(s.trim()) || matchRow(q, s)) break
    qs.unshift(s.trim())
    q--
  }

  return {
    question: qs.length ? qs.reduce((a, b) => joinWrapped(a, b), '') : null,
    choices,
    cursor: rows.indexOf(markers[0]),
    footer,
  }
}

/**
 * 點第 `target` 項要送進 pane 的鍵。
 *
 * 走 **↓／↑ × n + Enter**，不走「直接按數字」：腳註上寫的就是這兩顆
 * （`Enter to select · ↑/↓ to navigate`），每一種編號選單都吃，超過 9 項也還算得出來；
 * 而數字鍵是否直接選取各家 TUI 不一（claude 的滿意度問卷還要再補一個 Enter，
 * 見 `daemon/src/tui_prompts.rs`），猜錯就等於替使用者打了一個字進去。
 *
 * 方向是從游標往目標算的，中途不會走到頭尾，所以「到頂會不會繞回最後一項」這種各家不同的
 * 行為碰不到。呼叫端負責在送出**前**重讀一次畫面，用當下的 `cursor` 算，不要用一秒前的。
 */
export function keysToSelect(menu: TuiChoiceMenu, target: number): string[] {
  const delta = target - menu.cursor
  return [...Array(Math.abs(delta)).fill(delta > 0 ? 'down' : 'up'), 'enter']
}

/**
 * 兩張快照上是不是同一份選單。送鍵前拿它比對「我剛剛看到的那一份還在嗎」——不在就什麼都
 * 不送，因為 ↓ 的次數是相對的，畫面換了就會按到別的東西。
 */
export function sameChoices(a: TuiChoiceMenu, b: TuiChoiceMenu): boolean {
  return (
    a.choices.length === b.choices.length &&
    a.choices.every((c, i) => c.number === b.choices[i].number && c.title === b.choices[i].title)
  )
}
