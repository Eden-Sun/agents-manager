
import { splitPreviewPane } from './tuiPreviewPane.ts'
import { ASYNC_QUESTION_FOOTER as ASYNC_FOOTER } from './codexAsyncQuestion.ts'

export interface TuiChoice {
  number: number
  title: string
  detail: string
  current: boolean
  checked: boolean | null
  preview: string
}

export interface TuiTab {
  label: string
  done: boolean
  submit: boolean
}

export interface TuiChoiceMenu {
  question: string | null
  context: string[]
  command: string[]
  commandCut: boolean
  notes: string[]
  choices: TuiChoice[]
  cursor: number
  footer: boolean
  multi: boolean
  tabs: TuiTab[]
  tabAt: number | null
  review: { question: string; answer: string }[]
  submit: { after: number; current: boolean } | null
  asyncQueue?: boolean
}

const SCAN_LINES = 200

const TAIL_LIMIT = 10

const CONTEXT_LIMIT = 14

const COMMAND_LIMIT = 160

function isDashRule(s: string): boolean {
  const t = s.trim()
  return t !== '' && /^╌+$/.test(t)
}

const DETAIL_LIMIT = 8

const ITEM = /^(\s*)([❯›▶>]\s)?(\d{1,2})[.)](\s+)(\S.*)$/

const CHECKBOX = /^\[([ xX*✓✔☐☑☒])\]\s*(.*)$/

const SUBMIT = /^\s*([❯›▶>]\s*)?Submit\s*$/

const FOOTER = /enter to select|to navigate|esc to cancel/i

const TRANSCRIPT = /^[⏺⎿✽·✢✻✶*]/

const BULLET = /^\s+[·•]\s/

const ASKS = /[?？]$/

const PAGE_OF = /^\d+ of \d+$/

const DIALOG_TITLE = /^Session paused$/i

const TAB_MARK = /([☒☑☐✔✓])\s*([^☒☑☐✔✓←→]+)/g

const TAB_SCAN = 20

const REVIEW_Q = /^\s*[●•]\s+(\S.*)$/
const REVIEW_A = /^\s*(?:→|->)\s*(\S.*)$/

function stripBox(line: string): string {
  const s = line.replace(/\s+$/, '')
  const lead = /^\s*[│┃]\s?/.exec(s)
  if (!lead) return s
  return s.slice(lead[0].length).replace(/\s*[│┃]$/, '')
}

function isDivider(s: string): boolean {
  return s.trim() !== '' && /^[\s─-╿]+$/.test(s)
}

function indentOf(s: string): number {
  return s.length - s.trimStart().length
}

function hasMarker(s: string): boolean {
  return /^\s*[❯›▶>]\s/.test(s)
}

function joinWrapped(a: string, b: string): string {
  if (!a) return b
  if (!b) return a
  const cjk = /[⺀-鿿　-〿＀-￯]/
  return cjk.test(a[a.length - 1]) || cjk.test(b[0]) ? a + b : `${a} ${b}`
}

interface Row {
  line: number
  marker: boolean
  col: number
  number: number
  title: string
  checked: boolean | null
}

function matchRow(line: number, s: string): Row | null {
  const m = ITEM.exec(s)
  if (!m) return null
  const [, indent, marker, num, , rest] = m
  const box = CHECKBOX.exec(rest.trim())
  return {
    line,
    marker: Boolean(marker),
    col: indent.length + (marker?.length ?? 0),
    number: Number(num),
    title: (box ? box[2] : rest).trim(),
    checked: box ? box[1] !== ' ' && box[1] !== '☐' : null,
  }
}

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

function askedAbove(lines: string[], top: number): number {
  let found = -1
  for (let i = top - 1; i >= 0 && top - i <= CONTEXT_LIMIT; i--) {
    const s = lines[i]
    if (isDivider(s)) return found
    if (BULLET.test(s)) continue
    if (TRANSCRIPT.test(s.trim()) || matchRow(i, s) || parseTabs(s)) return -1
    if (found < 0 && ASKS.test(s.trim())) found = i
  }
  return -1
}

function dialogTitleAbove(lines: string[], top: number): number {
  for (let i = top - 1; i >= 0 && top - i <= CONTEXT_LIMIT; i--) {
    const s = lines[i].trim()
    if (DIALOG_TITLE.test(s)) return i
    if (isDivider(lines[i]) || TRANSCRIPT.test(s) || matchRow(i, lines[i]) || ASKS.test(s)) return -1
  }
  return -1
}

function dedentBlock(block: string[]): string[] {
  const out = block.map((l) => l.replace(/\s+$/, ''))
  while (out.length && !out[0]) out.shift()
  while (out.length && !out[out.length - 1]) out.pop()
  const pad = Math.min(...out.filter(Boolean).map(indentOf))
  return out.map((l) => l.slice(Math.min(pad, indentOf(l))))
}

function optionColumn(lines: string[]): number | null {
  let bottom: number | null = null
  for (let i = lines.length - 1; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (!row) continue
    if (hasMarker(lines[i])) return row.col
    bottom ??= row.col
  }
  return bottom
}

export function parseChoiceMenu(text: string | null | undefined): TuiChoiceMenu | null {
  if (!text) return null
  const raw = text.split('\n')
  const side = splitPreviewPane(raw)
  if (side && 'malformed' in side) return null
  const all = (side ? side.lines : raw).map(stripBox)
  const lines = all.slice(Math.max(0, all.length - SCAN_LINES))

  const optionCol = optionColumn(lines)
  let end = -1
  let footer = false
  let asyncQueue = false
  let tail = 0
  for (let i = lines.length - 1; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (row && row.col === optionCol) {
      end = i
      break
    }
    if (FOOTER.test(lines[i]) || ASYNC_FOOTER.test(lines[i])) footer = true
    if (ASYNC_FOOTER.test(lines[i])) asyncQueue = true
    if (lines[i].trim() === '' || isDivider(lines[i])) continue
    if (++tail > TAIL_LIMIT) return null
  }
  if (end < 0) return null
  if (lines.slice(end + 1).some((l) => /^[❯›]$/.test(l.replace(/[\s │┃╭╮╰╯─━▔]/g, '')))) return null

  const rows: Row[] = []
  let submitLine = -1
  let gap = 0
  for (let i = end; i >= 0; i--) {
    const row = matchRow(i, lines[i])
    if (row && row.col === optionCol) {
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

  if (side) {
    for (let i = end + 1; i < lines.length; i++) if (hasMarker(lines[i])) return null
  }

  const blockTop = rows[0].line
  const blockEnd = Math.max(end, submitLine)
  let markers = 0
  for (let i = blockTop; i <= blockEnd; i++) {
    const onOption = matchRow(i, lines[i])?.col === optionCol || i === submitLine
    if (onOption && hasMarker(lines[i])) markers++
  }
  if (markers !== 1) return null

  const offset = raw.length - lines.length
  const choicePreview = Boolean(side && lines.slice(blockTop, blockEnd + 1).some((_, i) =>
    side.lines[offset + blockTop + i] !== raw[offset + blockTop + i],
  ))

  const choices: TuiChoice[] = rows.map((row, idx) => {
    const stop = idx + 1 < rows.length ? rows[idx + 1].line : Math.min(lines.length, row.line + 1 + TAIL_LIMIT)
    let detail = ''
    for (let i = row.line + 1; i < stop; i++) {
      const s = lines[i]
      if (i === submitLine) continue
      if (s.trim() === '' || isDivider(s)) {
        if (idx + 1 < rows.length) continue
        break
      }
      if (idx + 1 === rows.length && (indentOf(s) < 2 || ASYNC_FOOTER.test(s))) break
      const piece = s.trim()
      detail = choicePreview && piece.startsWith('(') && detail ? `${detail} ${piece}` : joinWrapped(detail, piece)
    }
    const title = choicePreview ? (detail.startsWith('(') ? `${row.title} ${detail}` : joinWrapped(row.title, detail)) : row.title
    return {
      number: row.number,
      title,
      detail: choicePreview ? '' : detail,
      current: row.marker,
      checked: row.checked,
      preview: choicePreview && side && row.marker ? side.preview : '',
    }
  })

  let q = rows[0].line - 1
  while (q >= 0 && lines[q].trim() === '') q--
  const qs: string[] = []
  while (q >= 0 && qs.length < 4) {
    const s = lines[q]
    if (s.trim() === '' || isDivider(s) || TRANSCRIPT.test(s.trim()) || matchRow(q, s)) break
    if (parseTabs(s) || REVIEW_Q.test(s) || REVIEW_A.test(s) || PAGE_OF.test(s.trim())) break
    if (s.trim().startsWith('⚠')) break
    qs.unshift(s.trim())
    q--
  }

  let notes: string[] = []
  const titled = dialogTitleAbove(lines, rows[0].line)
  if (titled >= 0) {
    notes = dedentBlock(lines.slice(titled + 1, rows[0].line))
    q = titled - 1
    qs.splice(0, qs.length, lines[titled].trim())
  } else if (!ASKS.test(qs[qs.length - 1] ?? '')) {
    const asked = askedAbove(lines, rows[0].line)
    if (asked >= 0) {
      qs.length = 0
      q = asked
      while (q >= 0 && qs.length < 4 && lines[q].trim() && !isDivider(lines[q]) && !BULLET.test(lines[q])) {
        qs.unshift(lines[q].trim())
        q--
      }
      notes = dedentBlock(lines.slice(asked + 1, rows[0].line))
    }
  }

  const context: string[] = []
  let command: string[] = []
  let commandCut = false
  if (qs.length) {
    let c = q
    let blanks = 0
    let lead = 0
    while (c >= 0 && context.length < CONTEXT_LIMIT) {
      const s2 = lines[c]
      if (isDashRule(s2)) {
        let o = c - 1
        while (o >= 0 && command.length < COMMAND_LIMIT && !isDivider(lines[o])) {
          command.unshift(lines[o].replace(/\s+$/, ''))
          o--
        }
        commandCut = o < 0 || !isDivider(lines[o])
        while (command.length && command[command.length - 1].trim() === '') command.pop()
        break
      }
      if (isDivider(s2) || TRANSCRIPT.test(s2.trim()) || matchRow(c, s2) || parseTabs(s2)) break
      if (s2.trim() === '') {
        if (!context.length) {
          if (++lead > 4) break
          c--
          continue
        }
        if (++blanks > 1) break
        context.unshift('')
        c--
        continue
      }
      blanks = 0
      context.unshift(s2.replace(/\s+$/, ''))
      c--
    }
    while (context.length && context[0].trim() === '') context.shift()
    while (context.length && context[context.length - 1].trim() === '') context.pop()
  }

  let tabLine = -1
  for (let i = rows[0].line - 1; i >= 0 && rows[0].line - i <= TAB_SCAN; i--) {
    if (isDivider(lines[i]) || TRANSCRIPT.test(lines[i].trim())) break
    if (parseTabs(lines[i])) {
      tabLine = i
      break
    }
  }
  const tabs = (tabLine >= 0 ? parseTabs(lines[tabLine]) : null) ?? []

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
    if (review.length && indentOf(lines[i]) >= 3 && lines[i].trim() && !matchRow(i, lines[i]) && !REVIEW_Q.test(lines[i])) {
      const last = review[review.length - 1]
      last.answer = joinWrapped(last.answer, lines[i].trim())
    }
  }

  const submitIdx = tabs.findIndex((t) => t.submit)
  const onSubmitPage = review.length > 0 || choices.some((c) => /^submit answers$/i.test(c.title))
  const undone = tabs.findIndex((t) => !t.done && !t.submit)
  const at = onSubmitPage && submitIdx >= 0 ? submitIdx : undone >= 0 ? undone : submitIdx

  const submitAfter = submitLine < 0 ? -1 : rows.filter((r) => r.line < submitLine).length - 1

  return {
    question: qs.length ? qs.reduce((a, b) => joinWrapped(a, b), '') : null,
    context,
    command,
    commandCut,
    notes,
    choices,
    cursor: rows.findIndex((r) => r.marker),
    footer,
    multi: choices.some((c) => c.checked !== null),
    tabs,
    tabAt: at >= 0 ? at : null,
    review,
    submit: submitLine < 0 ? null : { after: submitAfter, current: hasMarker(lines[submitLine]) },
    ...(asyncQueue ? { asyncQueue } : {}),
  }
}

export type WalkTarget = number | 'submit'

function walkRows(menu: TuiChoiceMenu): WalkTarget[] {
  const rows: WalkTarget[] = menu.choices.map((_, i) => i)
  if (menu.submit) rows.splice(menu.submit.after + 1, 0, 'submit')
  return rows
}

export function keysToMove(menu: TuiChoiceMenu, target: WalkTarget): string[] | null {
  const rows = walkRows(menu)
  const from = menu.submit?.current ? rows.indexOf('submit') : rows.indexOf(menu.cursor)
  const to = rows.indexOf(target)
  if (from < 0 || to < 0) return null
  const delta = to - from
  return Array.from({ length: Math.abs(delta) }, () => (delta > 0 ? 'down' : 'up'))
}

export function keysToSelect(menu: TuiChoiceMenu, target: number): string[] {
  return [...(keysToMove(menu, target) ?? []), 'enter']
}

export function isActionChoice(c: Pick<TuiChoice, 'title'>): boolean {
  return isTypeSomething(c) || /^chat about this$/i.test(c.title.trim())
}

export function isTypeSomething(c: Pick<TuiChoice, 'title'>): boolean {
  return /^type something\.?$/i.test(c.title.trim())
}

export function typedAnswerHere(menu: { multi: boolean; asyncQueue?: boolean }, c: Pick<TuiChoice, 'title'>): boolean {
  return isTypeSomething(c) || (menu.asyncQueue === true && /^other$/i.test(c.title.trim()))
}

export function customAnswerChecked(choice: TuiChoice, text: string): boolean {
  return choice.checked === true && customAnswerShown(choice, text)
}

export function customAnswerShown(choice: TuiChoice, text: string): boolean {
  const squash = (s: string) => s.replace(/\s+/g, '')
  const want = squash(text)
  if (!want) return false
  const visible = `${choice.title.replace(/\s*✔\s*$/, '')} ${choice.detail ?? ''}`.trim()
  const shown = squash(visible)
  if (shown.includes(want)) return true
  if (!text.includes('\n') || !/…$/.test(visible)) return false
  const seen = squash(visible.replace(/…$/, ''))
  const first = squash(text.split('\n')[0] ?? '')
  return seen.length > first.length && want.startsWith(seen)
}

export function sameChoices(a: TuiChoiceMenu, b: TuiChoiceMenu): boolean {
  return (
    a.question === b.question &&
    a.choices.length === b.choices.length &&
    a.choices.every((c, i) => c.number === b.choices[i].number && c.title === b.choices[i].title)
  )
}
