/**
 * 從終端快照裡認出 agent 停在上面的「編號選單」，好讓 web 把它畫成可以點的清單
 * （2026-09-12 使用者：手機上要選第 4 項得按三次 ↓ 再 Enter，而且選項文字被終端寬度截掉）。
 *
 * 認得出兩種形狀，都是 claude 2.x 的 AskUserQuestion（真機 185 欄快照）：
 *
 * **單選**
 * ```text
 * robinstech-carbis-web 要先推進哪一塊？
 *
 * ❯ 1. 先把 WIP commit 掉（建議）
 *      57 個檔案未 commit，包含 #21 的評價 demo…
 *   2. 接 console 的回饋 API（#22）
 *
 * Enter to select · ↑/↓ to navigate · Esc to cancel
 * ```
 *
 * **多分頁 ＋ 多選**（2026-09-12 第三輪：使用者回報這種「第二個分頁」整塊解析不出來）
 * ```text
 * ←  ☒ 編輯方式  ☐ 功能  ✔ Submit  →
 *
 * 除了看文章，還需要哪些功能？
 *
 * ❯ 1. [ ] 站內搜尋、分類、標籤
 *   静態可行：建置時產索引…              ← 說明只縮排 2（跟編號同一欄），不是 5
 *   2. [ ] 訂閱電子報、留言、按讚
 *   5. [ ] Type something
 *      Submit                            ← 沒有編號、但游標走得到的一列
 * ──────────────────────────────────────  ← 分隔線把 6 切到後面
 *   6. Chat about this
 *
 * Enter to select · Tab/Arrow keys to navigate · Esc to cancel
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
  /** 第一行的選項本文（去掉游標記號、編號與 `[ ]`）。 */
  title: string
  /** 底下的說明，終端折行的地方已經接回一段。沒有就是空字串。 */
  detail: string
  /** 游標現在停在這一項。 */
  current: boolean
  /** 多選時的勾選狀態（`[x]` = true）；單選選單一律 `null`。 */
  checked: boolean | null
}

/** 上方那條分頁列的一格：一題問卷裡的一題。 */
export interface TuiTab {
  label: string
  /** `☒`＝這一題答過了，`☐`＝還沒。 */
  done: boolean
  /** `✔ Submit`＝整份問卷的送出頁，不是一題。 */
  submit: boolean
}

export interface TuiChoiceMenu {
  /** 選單上方那句問題（折行已接回）；上面沒有可辨識的問句就是 `null`。 */
  question: string | null
  choices: TuiChoice[]
  /** 游標停在 `choices` 的第幾個；停在清單裡沒有編號的那一列（`Submit`）時是 `-1`。 */
  cursor: number
  /** 畫面上有 claude 的腳註（`Enter to select · …`）。只是信心指標。 */
  footer: boolean
  /** 選項帶 `[ ]`／`[x]`：可以複選，點一下是切換勾選，不是送出。 */
  multi: boolean
  /** 上方那條分頁列；不是多題問卷就是空陣列。 */
  tabs: TuiTab[]
  /**
   * 猜的「現在停在第幾個分頁」：第一個還沒答的；全部答完就是 `✔ Submit` 那格。
   *
   * 終端上那一格是用顏色標的，快照只剩純文字，所以這是**推測**不是讀出來的。UI 要照實說。
   */
  tabAt: number | null
  /**
   * review／confirm 畫面中段那段「每題 → 目前答案」（`● 問題` / `→ 答案`）。
   * 一般選項頁沒有這段，就是空陣列。
   */
  review: { question: string; answer: string }[]
  /** 清單裡那一列沒有編號的 `Submit`（多選才有）。`after` 是它排在第幾個選項後面。 */
  submit: { after: number; current: boolean } | null
}

/** 只看畫面底下這麼多行：選單畫在輸入列的位置，正文裡的編號清單不算。 */
const SCAN_LINES = 60

/** 最後一個選項到畫面底之間，最多容得下這麼多行有字的東西（腳註、輸入框、狀態列）。 */
const TAIL_LIMIT = 10

/** 兩個編號之間最多夾這麼多行說明；再多就不是選項說明，是別的東西混進來了。 */
const DETAIL_LIMIT = 8

/** 一行選項：`❯ 1. 標題` / `  2. 標題` / `> 1. 標題`。 */
const ITEM = /^(\s*)([❯›▶>]\s)?(\d{1,2})[.)](\s+)(\S.*)$/

/** 選項本文前面的核取方塊（多選）。 */
const CHECKBOX = /^\[([ xX*✓✔])\]\s*(.*)$/

/** 清單裡那一列沒有編號的 `Submit`（多選問卷的送出列）。 */
const SUBMIT = /^\s*([❯›▶>]\s*)?Submit\s*$/

/** claude 的腳註。有它就幾乎確定是選單，但沒有也還是可能是（權限框就沒有）。 */
const FOOTER = /enter to select|to navigate|esc to cancel/i

/** claude 印工具呼叫與結果的行首符號：問題往上找時撞到它就停，那已經是正文了。 */
const TRANSCRIPT = /^[⏺⎿✽·✢✻✶*]/

/** 分頁列上的記號：`☒` 答過、`☐` 還沒、`✔` 送出頁。 */
const TAB_MARK = /([☒☑☐✔✓])\s*([^☒☑☐✔✓←→]+)/g

/**
 * 分頁列往上找幾行。
 *
 * 一般選項頁它就貼在問題正上方，但 review／confirm 那頁中間還隔著一整段「每題 → 答案」
 * （2026-09-12 第七輪：使用者在最後那頁點不到分頁，回不去改答案，就是因為原本只看問題正
 * 上方那一行）。所以改成往上找一段，取**最靠近**的那條，遇到分隔線或正文符號就停。
 */
const TAB_SCAN = 20

/** review 那段：`● 問題` 一列、`→ 答案` 一列。 */
const REVIEW_Q = /^\s*[●•]\s+(\S.*)$/
const REVIEW_A = /^\s*(?:→|->)\s*(\S.*)$/

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

function hasMarker(s: string): boolean {
  return /^\s*[❯›▶>]\s/.test(s)
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

/**
 * 分頁列（`←  ☒ 編輯方式  ☐ 功能  ✔ Submit  →`）。不是這種行就回 `null`。
 *
 * **哪一格是「現在這一題」讀不出來**：終端上那是用顏色標的，而快照只剩純文字。所以這裡只
 * 回報每一格答過沒有，「目前在第幾題」交給 UI 用「第一個還沒答的」去猜並且說明是猜的。
 */
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

  // 2. 繼續往上收，收到編號 1 為止。
  //
  // 夾在兩個編號之間的非編號行一律當成上一項的說明（最多 DETAIL_LIMIT 行）——縮排靠不住：
  // 單選那份的說明縮到本文那一欄（5），多選這份只縮到編號那一欄（2），兩者都真實存在。
  // 唯一要挑出來的是沒有編號但游標走得到的 `Submit` 列。
  const rows: Row[] = []
  let submitLine = -1
  let gap = 0
  for (let i = end; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (row) {
      // 往上走編號要遞減：跳號、重號都當成「這不是同一份選單」。
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
    // 還沒收到任何編號就遇到雜訊 → 這不是選單的一部分。
    if (!rows.length || ++gap > DETAIL_LIMIT) break
  }
  rows.reverse()

  if (rows.length < 2 || rows[0].number !== 1) return null

  // `Submit` 那一列也可能排在**最後一個編號之後**（那一頁沒有 `Chat about this` 時就是這樣）。
  // 往上走的時候看不到它，所以再往下看幾行；碰到腳註或第二個空白行就停。
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

  // 游標記號在整塊裡**剛好一個**：零個代表游標捲出畫面（算不出走幾格），兩個以上代表這根本
  // 不是選單（例如正文裡的 `> 1. …` 引用）。記號可能落在 `Submit` 那一列上，那也算一個。
  const blockTop = rows[0].line
  const blockEnd = Math.max(end, submitLine)
  let markers = 0
  for (let i = blockTop; i <= blockEnd; i++) if (hasMarker(lines[i])) markers++
  if (markers !== 1) return null

  // 3. 每一項底下的說明接成一段。最後一項往下收到空白行／分隔線為止。
  const choices: TuiChoice[] = rows.map((row, idx) => {
    const stop = idx + 1 < rows.length ? rows[idx + 1].line : Math.min(lines.length, row.line + 1 + TAIL_LIMIT)
    let detail = ''
    for (let i = row.line + 1; i < stop; i++) {
      const s = lines[i]
      if (i === submitLine) continue
      if (s.trim() === '' || isDivider(s)) {
        // 選項之間夾的空白／分隔線不算結束（真機快照在 5 與 6 之間就有一條）；
        // 最後一項後面的空白行則是選單的下緣。
        if (idx + 1 < rows.length) continue
        break
      }
      if (idx + 1 === rows.length && indentOf(s) < 2) break
      detail = joinWrapped(detail, s.trim())
    }
    return { number: row.number, title: row.title, detail, current: row.marker, checked: row.checked }
  })

  // 4. 第一項上面那句問題（先跳過空白行，再往上收連續的幾行）。
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

  // 5. 分頁列：從選單起點往上找**最靠近**的那一條，不是只看問題正上方那一行。
  let tabLine = -1
  for (let i = rows[0].line - 1; i >= 0 && rows[0].line - i <= TAB_SCAN; i--) {
    if (isDivider(lines[i]) || TRANSCRIPT.test(lines[i].trim())) break
    if (parseTabs(lines[i])) {
      tabLine = i
      break
    }
  }
  const tabs = (tabLine >= 0 ? parseTabs(lines[tabLine]) : null) ?? []

  // 6. 分頁列與選單之間那段 review（`● 問題` / `→ 答案`），review 頁才有。
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
    }
  }

  // 哪一格是現在這一題讀不出來（終端只用顏色標），猜第一個還沒答的；全答完就是送出頁。
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

/** 游標走得到的一列：編號選項，或清單裡那個沒有編號的 `Submit`。 */
export type WalkTarget = number | 'submit'

/**
 * 把「游標走得到的列」照畫面順序排出來。
 *
 * `Submit` 夾在選項中間（真機是第 5 與第 6 之間），↓ 要按幾次得把它算進去，不然點第 6 項
 * 會停在 Submit 上。清單裡若還有別的沒有編號、我們認不得的列，`walk` 的重讀比對會補正。
 */
function walkRows(menu: TuiChoiceMenu): WalkTarget[] {
  const rows: WalkTarget[] = menu.choices.map((_, i) => i)
  if (menu.submit) rows.splice(menu.submit.after + 1, 0, 'submit')
  return rows
}

/**
 * 從現在的游標位置走到 `target` 要送的方向鍵（**不含 Enter**）。
 *
 * 走 ↓／↑ 而不是按數字：腳註寫的就是這幾顆（`Enter to select · ↑/↓`／`Tab/Arrow keys to
 * navigate`），每一種編號選單都吃、超過 9 項也算得出來；數字鍵是否直接選取各家 TUI 不一
 * （claude 的滿意度問卷還要再補一個 Enter，見 `daemon/src/tui_prompts.rs`）。
 *
 * 方向是從游標往目標算的，中途不會走到頭尾，所以「到頂會不會繞回最後一項」碰不到。
 * 算不出來（游標不在任何認得的列上）回 `null`——呼叫端要嘛重讀重算，要嘛什麼都不送。
 */
export function keysToMove(menu: TuiChoiceMenu, target: WalkTarget): string[] | null {
  const rows = walkRows(menu)
  const from = menu.submit?.current ? rows.indexOf('submit') : rows.indexOf(menu.cursor)
  const to = rows.indexOf(target)
  if (from < 0 || to < 0) return null
  const delta = to - from
  return Array.from({ length: Math.abs(delta) }, () => (delta > 0 ? 'down' : 'up'))
}

/**
 * 單選選單「點第 `target` 項」的一整批鍵：走過去再 Enter。
 *
 * 單選沒有 `Submit` 那種沒編號的列，走幾格算得準，所以一批送完就好（2026-09-12 第一輪實測
 * 過的路徑）。多選要切換勾選、要送出，走一步驗一步，見 `BlockedChoices`。
 */
export function keysToSelect(menu: TuiChoiceMenu, target: number): string[] {
  return [...(keysToMove(menu, target) ?? []), 'enter']
}

/** `Type something.` / `Chat about this`：編號列，但不是勾選項。 */
export function isActionChoice(c: Pick<TuiChoice, 'title'>): boolean {
  return /^type something\.?$/i.test(c.title.trim()) || /^chat about this$/i.test(c.title.trim())
}

/**
 * 兩張快照上是不是同一份選單。送鍵前拿它比對「我剛剛看到的那一份還在嗎」——不在就什麼都
 * 不送，因為 ↓ 的次數是相對的，畫面換了就會按到別的東西。
 *
 * 比的是題目與選項本文，**不比勾選狀態與游標**：那兩樣本來就是我們自己按出來的變化。
 */
export function sameChoices(a: TuiChoiceMenu, b: TuiChoiceMenu): boolean {
  return (
    a.question === b.question &&
    a.choices.length === b.choices.length &&
    a.choices.every((c, i) => c.number === b.choices[i].number && c.title === b.choices[i].title)
  )
}
