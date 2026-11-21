import { t } from '../lib/i18n'
import {
  customAnswerChecked,
  customAnswerShown,
  isTypeSomething,
  keysToMove,
  parseChoiceMenu,
  type TuiChoiceMenu,
  type WalkTarget,
} from './tuiChoices.ts'

export interface Io {
  read: () => Promise<TuiChoiceMenu | null>
  send: (keys: string[]) => Promise<void>
  wait: (ms: number) => Promise<void>
  paste: (text: string) => Promise<void>
}

export async function readChoiceMenu(read: () => Promise<string>): Promise<TuiChoiceMenu | null> {
  return parseChoiceMenu(await read())
}

export interface DraftPage {
  tab: number
  label: string
  question: string | null
  choices: TuiChoiceMenu['choices']
  multi: boolean
  hasSubmitRow: boolean
  review: TuiChoiceMenu['review']
  isSubmit: boolean
  answered?: number
}

export interface Draft {
  pages: DraftPage[]
  startTab: number
  tabs: TuiChoiceMenu['tabs']
}

const SETTLE_TRIES = 6
const SETTLE_STEP = 120

export const MIN_TABS = 2

function pageOf(menu: TuiChoiceMenu, tab: number, label: string): DraftPage {
  return {
    tab,
    label,
    question: menu.question,
    choices: menu.choices,
    multi: menu.multi,
    hasSubmitRow: Boolean(menu.submit),
    review: menu.review,
    isSubmit: isSubmitMenu(menu),
  }
}

function submitPage(tab: number, label: string): DraftPage {
  return { tab, label, question: null, choices: [], multi: false, hasSubmitRow: false, review: [], isSubmit: true }
}

export function pageKey(menu: TuiChoiceMenu): string {
  return JSON.stringify([
    menu.question,
    menu.choices.map((c) => [c.number, c.title]),
    menu.tabs.map((t) => [t.label, t.done]),
    menu.review,
  ])
}

export async function moveTab(io: Io, dir: 1 | -1, from: TuiChoiceMenu, signal?: AbortSignal): Promise<TuiChoiceMenu | null> {
  const before = pageKey(from)
  for (const key of dir > 0 ? ['right', 'tab'] : ['left', 'shift+tab']) {
    if (signal?.aborted) return null
    await io.send([key])
    for (let i = 0; i < SETTLE_TRIES; i++) {
      if (signal?.aborted) return null
      await io.wait(SETTLE_STEP)
      const now = await io.read()
      if (now && pageKey(now) !== before) return now
    }
    if (from.question === null) return null
  }
  return null
}

let queue: Promise<unknown> = Promise.resolve()

export function preload(
  io: Io,
  _start: TuiChoiceMenu,
  onProgress?: (done: number, total: number) => void,
  signal?: AbortSignal,
): Promise<Draft | null> {
  const run = queue.then(async () => {
    if (signal?.aborted) return null
    const fresh = await io.read().catch(() => null)
    if (!fresh || signal?.aborted) return null
    return preloadOnce(io, fresh, onProgress, signal)
  })
  queue = run.catch(() => null)
  return run
}

async function preloadOnce(
  io: Io,
  start: TuiChoiceMenu,
  onProgress?: (done: number, total: number) => void,
  signal?: AbortSignal,
): Promise<Draft | null> {
  const tabs = start.tabs
  const n = tabs.length
  if (n < MIN_TABS) return null
  let last = n - 1
  while (last > 0 && tabs[last].submit) last -= 1
  const reviewNow = start.review

  let cur: TuiChoiceMenu = start
  const submitAt = tabs.findIndex((t) => t.submit)
  let at = isSubmitMenu(start) ? (submitAt >= 0 ? submitAt : n - 1) : (start.tabAt ?? -1)
  if (at < 0 || at >= n) return null
  while (at > last || isSubmitMenu(cur)) {
    const next = await moveTab(io, -1, cur, signal)
    if (!next) return null
    at -= 1
    cur = next
  }
  const startTab = at
  if (startTab > last || startTab < 0) return null

  const startPage = pageOf(cur, startTab, tabs[startTab].label)
  const pages: (DraftPage | undefined)[] = new Array<DraftPage | undefined>(n)
  let done = 0
  const note = () => onProgress?.(++done, n)

  pages[at] = startPage
  note()
  for (let t = last + 1; t < n; t++) {
    pages[t] = submitPage(t, tabs[t].label)
    note()
  }

  for (const dir of [-1, 1] as const) {
    const stop = dir < 0 ? 0 : last
    while (at !== stop) {
      const next = await moveTab(io, dir, cur, signal)
      if (!next) return null
      at += dir
      cur = next
      const had = pages[at]
      pages[at] = pageOf(cur, at, tabs[at]?.label ?? t('第 {n} 題', { n: at + 1 }))
      if (!had) note()
    }
  }

  let target = tabs.findIndex((t, k) => k <= last && !t.submit && !t.done)
  if (target < 0) target = 0
  while (at > target) {
    const next = await moveTab(io, -1, cur, signal)
    if (!next) return null
    at -= 1
    cur = next
  }

  const full = pages.filter((p): p is DraftPage => Boolean(p))
  if (full.length !== n) return null
  return {
    pages: full.map((p) => (p.isSubmit ? p : { ...p, answered: answeredIndex(p, reviewNow) })),
    startTab: target,
    tabs,
  }
}

function answeredIndex(page: DraftPage, review: TuiChoiceMenu['review']): number {
  if (page.multi || page.isSubmit || !page.question) return -1
  const row = review.find((r) => squash(r.question) === squash(page.question ?? ''))
  const got = squash(row?.answer ?? '')
  if (!got) return -1
  return page.choices.findIndex((c) => !isTypeSomething(c) && squash(c.title) === got)
}

export function wantOf(page: DraftPage): boolean[] {
  if (!page.multi) return page.choices.map((_, j) => j === page.answered)
  return page.choices.map((c) => c.checked === true)
}

export function radioPick(want: boolean[] | undefined): number {
  if (!want) return -1
  const i = want.findIndex(Boolean)
  return i
}

export function pageNeedsCommit(page: DraftPage, want: boolean[] | undefined): boolean {
  if (page.isSubmit) return false
  if (!page.multi) return radioPick(want) >= 0 && radioPick(want) !== (page.answered ?? -1)
  return togglesFor(page.choices, want ?? wantOf(page)).length > 0
}

export function typeToPaste(page: DraftPage, want: boolean[] | undefined): number {
  return page.choices.findIndex((c, j) => Boolean(want?.[j]) && !c.checked && isTypeSomething(c))
}

export function togglesFor(now: TuiChoiceMenu['choices'], want: boolean[]): number[] {
  const out: number[] = []
  now.forEach((c, i) => {
    if (c.checked === null) return
    if (c.checked !== Boolean(want[i])) out.push(i)
  })
  return out
}

export function locate(draft: Draft, now: TuiChoiceMenu): number | null {
  const pages = draft.pages.filter((p) => !p.isSubmit)
  const exact = pages.filter((p) => samePage(p, now))
  const loose = now.question === null ? [] : pages.filter((p) => p.question === now.question && p.choices.length === now.choices.length)
  const hits = exact.length ? exact : loose
  if (hits.length === 1) return hits[0].tab
  return isSubmitMenu(now) ? (draft.pages.find((p) => p.isSubmit)?.tab ?? null) : null
}

export function isSubmitMenu(now: TuiChoiceMenu): boolean {
  return now.review.length > 0 || now.choices.some((c) => /^submit answers$/i.test(c.title))
}

export function samePage(page: DraftPage, now: TuiChoiceMenu): boolean {
  return (
    page.question === now.question &&
    page.choices.length === now.choices.length &&
    page.choices.every((c, i) => c.title === now.choices[i].title && c.number === now.choices[i].number)
  )
}

async function walkTo(io: Io, target: WalkTarget, page: DraftPage): Promise<TuiChoiceMenu | null> {
  for (let i = 0; i < 3; i++) {
    const now = await io.read()
    if (!now || !samePage(page, now)) return null
    const at: WalkTarget | null = now.submit?.current ? 'submit' : now.cursor >= 0 ? now.cursor : null
    if (at === target) return now
    const keys = at === null ? ['up'] : (keysToMove(now, target) ?? [])
    if (!keys.length) return null
    await io.send(keys)
    await io.wait(SETTLE_STEP * 2)
  }
  return null
}

async function pressUntil(io: Io, key: string, done: (now: TuiChoiceMenu | null) => boolean): Promise<boolean> {
  await io.send([key])
  for (let i = 0; i < SETTLE_TRIES; i++) {
    await io.wait(SETTLE_STEP)
    if (done(await io.read())) return true
  }
  return false
}

async function settleAfterEnter(io: Io, before: TuiChoiceMenu): Promise<TuiChoiceMenu | null> {
  let last: TuiChoiceMenu | null = null
  for (let i = 0; i < SETTLE_TRIES; i++) {
    await io.wait(SETTLE_STEP * (i === 0 ? 3 : 1))
    const now = await io.read()
    if (!now) continue
    last = now
    if (now.question !== before.question || now.review.length !== before.review.length) return now
  }
  return last
}

const squash = (s: string) => s.replace(/\s+/g, '')

function alreadyAnswered(page: DraftPage, want: boolean[] | undefined, custom: string, review: TuiChoiceMenu['review']): boolean {
  if (page.multi || page.isSubmit) return false
  const idx = radioPick(want)
  const c = idx >= 0 ? page.choices[idx] : undefined
  if (!c || !page.question) return false
  const row = review.find((r) => squash(r.question) === squash(page.question ?? ''))
  if (!row) return false
  const got = squash(row.answer)
  const text = squash(isTypeSomething(c) ? custom : c.title)
  return Boolean(text) && Boolean(got) && (isTypeSomething(c) ? got.includes(text) : got === text)
}

export interface CommitResult {
  ok: boolean
  error?: string
  at?: number
}

export async function commit(
  io: Io,
  draft: Draft,
  want: boolean[][],
  onProgress?: (done: number, total: number) => void,
  custom: string[] = [],
): Promise<CommitResult> {
  const wanted = draft.pages.map((p, i) => ({ p, i })).filter(({ p, i }) => {
    if (pageNeedsCommit(p, want[i])) return true
    return (
      p.multi &&
      p.hasSubmitRow &&
      !draft.tabs[p.tab]?.done &&
      (want[i] ?? wantOf(p)).some(Boolean)
    )
  })

  for (const { p, i } of wanted) {
    const typing = p.multi ? typeToPaste(p, want[i]) : radioPick(want[i])
    if (typing >= 0 && isTypeSomething(p.choices[typing] ?? { title: '' }) && !(custom[p.tab] ?? '').trim()) {
      return {
        ok: false,
        error: t('第 {n} 題選了「Type something.」但沒有打字。請打一段或改選別項。', { n: p.tab + 1 }),
        at: i,
      }
    }
  }

  let cur = await io.read()
  if (!cur) return { ok: false, error: t('讀不到畫面，什麼都沒送出。') }
  if (cur.tabs.length !== draft.tabs.length) {
    return { ok: false, error: t('畫面已經變了（分頁數不一樣），整批都沒送出，請重新讀取。') }
  }
  const start = locate(draft, cur)
  if (start === null && !isSubmitMenu(cur)) {
    return { ok: false, error: t('分不出終端停在哪一頁，什麼都沒送出，請重新讀取'), at: 0 }
  }
  let at = start ?? draft.startTab

  const onReview = isSubmitMenu(cur)
  const reviewNow = cur.review
  const todo = onReview
    ? wanted.filter(({ p, i }) => !alreadyAnswered(p, want[i], (custom[p.tab] ?? '').trim(), reviewNow))
    : wanted

  let done = 0
  const total = todo.length + 1

  for (const { p, i } of todo) {
    while (at !== p.tab) {
      const dir = p.tab > at ? 1 : -1
      const next = await moveTab(io, dir, cur)
      if (!next) return { ok: false, error: t('走不到第 {n} 題（{label}），後面的都沒送出。', { n: p.tab + 1, label: p.label }), at: i }
      cur = next
      const found = locate(draft, cur)
      if (found === null) {
        return { ok: false, error: t('導覽後的畫面對不上草稿，分不出終端停在哪一頁（題目或選項跟讀進來時不一樣），後面的都沒送出，請看終端。'), at: i }
      }
      at = found
    }
    if (!samePage(p, cur)) {
      return { ok: false, error: t('第 {n} 題（{label}）的選項跟讀進來時不一樣，沒有送出。', { n: p.tab + 1, label: p.label }), at: i }
    }

    if (!p.multi) {
      const idx = radioPick(want[i])
      if (idx < 0) continue
      const landed = await walkTo(io, idx, p)
      if (!landed) return { ok: false, error: t('第 {n} 題的游標走不到第 {item} 項，停在這裡。', { n: p.tab + 1, item: idx + 1 }), at: i }
      if (isTypeSomething(p.choices[idx] ?? { title: '' })) {
        const text = (custom[p.tab] ?? '').trim()
        await io.paste(text)
        let shown = false
        for (let t = 0; t < SETTLE_TRIES; t++) {
          await io.wait(SETTLE_STEP)
          const now = await io.read()
          if (now && now.choices[idx] && customAnswerShown(now.choices[idx], text)) {
            shown = true
            break
          }
        }
        if (!shown) {
          return { ok: false, error: t('第 {n} 題的自訂文字沒有出現在畫面上，沒有按 Enter。', { n: p.tab + 1 }), at: i }
        }
      }
      await io.send(['enter'])
      const now = await settleAfterEnter(io, cur)
      if (!now) return { ok: false, error: t('第 {n} 題送出後讀不到畫面，請重新讀取。', { n: p.tab + 1 }), at: i }
      cur = now
      at = locate(draft, now) ?? at
      onProgress?.(++done, total)
      continue
    }

    let page = p
    const typeIdx = typeToPaste(p, want[i])
    if (typeIdx >= 0) {
      const text = (custom[p.tab] ?? '').trim()
      const landed = await walkTo(io, typeIdx, p)
      if (!landed) return { ok: false, error: t('第 {n} 題的游標走不到第 {item} 項，停在這裡。', { n: p.tab + 1, item: typeIdx + 1 }), at: i }
      await io.paste(text)
      let typed: TuiChoiceMenu | null = null
      for (let t = 0; t < SETTLE_TRIES; t++) {
        await io.wait(SETTLE_STEP)
        const now = await io.read()
        if (now && now.choices[typeIdx] && customAnswerChecked(now.choices[typeIdx], text)) {
          typed = now
          break
        }
      }
      if (!typed) {
        return { ok: false, error: t('第 {n} 題的自訂文字沒有出現在畫面上（或沒勾起來），停在這裡。', { n: p.tab + 1 }), at: i }
      }
      cur = typed
      page = { ...p, choices: p.choices.map((c, j) => (j === typeIdx ? { ...c, title: typed.choices[typeIdx].title, checked: true } : c)) }
    }

    for (const idx of togglesFor(cur.choices, want[i])) {
      const landed = await walkTo(io, idx, page)
      if (!landed) return { ok: false, error: t('第 {n} 題的游標走不到第 {item} 項，停在這裡。', { n: p.tab + 1, item: idx + 1 }), at: i }
      const before = landed.choices[idx].checked
      const flipped = (now: TuiChoiceMenu | null) => Boolean(now && now.choices[idx]?.checked !== before)
      if (await pressUntil(io, 'space', flipped)) continue
      const n = p.choices[idx].number
      if (n <= 9 && (await pressUntil(io, String(n), flipped))) continue
      return { ok: false, error: t('第 {tab} 題的第 {item} 項按不動（space 與數字鍵都沒反應），停在這裡。', { tab: p.tab + 1, item: n }), at: i }
    }

    const after = await io.read()
    if (!after || !samePage(page, after)) {
      return { ok: false, error: t('第 {n} 題送完之後畫面對不上，後面的都沒送出。', { n: p.tab + 1 }), at: i }
    }
    if (togglesFor(after.choices, want[i]).length) {
      return { ok: false, error: t('第 {n} 題的勾選沒有全部生效，後面的都沒送出。', { n: p.tab + 1 }), at: i }
    }
    cur = after
    onProgress?.(++done, total)

    if (p.hasSubmitRow) {
      const landed = await walkTo(io, 'submit', page)
      if (!landed?.submit?.current) {
        return { ok: false, error: t('第 {n} 題的游標沒有停在 Submit 上，沒有按下去。', { n: p.tab + 1 }), at: i }
      }
      await io.send(['enter'])
      const now = await settleAfterEnter(io, cur)
      if (!now) return { ok: false, error: t('第 {n} 題送出後讀不到畫面，請重新讀取。', { n: p.tab + 1 }), at: i }
      cur = now
      at = locate(draft, now) ?? at
    }
  }

  const submitTab = draft.pages.findIndex((p) => p.isSubmit)
  const target = submitTab >= 0 ? draft.pages[submitTab].tab : draft.tabs.findIndex((t) => t.submit)
  if (target < 0) return { ok: false, error: t('找不到送出頁，勾選已經送出去了，最後一步請自己按。') }
  while (at !== target) {
    const dir = target > at ? 1 : -1
    const next = await moveTab(io, dir, cur)
    if (!next) return { ok: false, error: t('走不到送出頁，勾選已經送出去了，最後一步請自己按。') }
    cur = next
    const found = locate(draft, cur)
    if (found === null) {
      return { ok: false, error: t('導覽後的畫面對不上草稿，分不出終端停在哪一頁（題目或選項跟讀進來時不一樣），勾選已經送出去了，最後一步請自己按。') }
    }
    at = found
  }
  const idx = cur.choices.findIndex((c) => /^submit answers$/i.test(c.title))
  if (idx < 0) return { ok: false, error: t('送出頁上找不到「Submit answers」，最後一步請自己按。') }
  const page = pageOf(cur, target, draft.tabs[target]?.label ?? 'Submit')
  const landed = await walkTo(io, idx, page)
  if (!landed || landed.cursor !== idx) return { ok: false, error: t('游標沒有停在「Submit answers」上，沒有按下去。') }
  await io.send(['enter'])
  onProgress?.(++done, total)
  let lastReadFailed = false
  for (let i = 0; i < SETTLE_TRIES; i++) {
    await io.wait(SETTLE_STEP * 2)
    let now: TuiChoiceMenu | null
    try {
      now = await io.read()
    } catch {
      lastReadFailed = true
      continue
    }
    lastReadFailed = false
    if (!now || !isSubmitMenu(now)) return { ok: true }
  }
  if (lastReadFailed) {
    return {
      ok: false,
      error: t('已送出「Submit answers」，但讀取終端失敗，無法確認是否交卷。請回終端檢查；若仍停在送出頁，再按 Enter。'),
    }
  }
  return {
    ok: false,
    error: t('已經送出「Submit answers」，但終端還停在送出頁（Enter 可能沒送到）。答案都填好了，請再按一次送出，或直接在終端按 Enter。'),
  }
}

export function parse(text: string | null | undefined): TuiChoiceMenu | null {
  return parseChoiceMenu(text)
}

export function isSurvey(menu: TuiChoiceMenu): boolean {
  return menu.tabs.length >= MIN_TABS && menu.tabAt !== null
}
