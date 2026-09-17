/**
 * 多分頁問卷：先讀完、離線作答、最後一次送出（2026-09-12 第六輪，使用者指定：「一開始就先
 * 預載入所有的分頁，而不是每一次連動，最後送出前再確認就好」）。
 *
 * preload 只送導覽鍵、走完回原頁，不能改到答案；作答只動本地 state；commit 逐頁比對同一份
 * 選單、只送差集（複選是 toggle），任何一步對不上就停。I/O 走 `Io` 注入以便測試。
 */
import {
  customAnswerShown,
  isTypeSomething,
  keysToMove,
  MULTI_TYPE_HINT,
  parseChoiceMenu,
  type TuiChoiceMenu,
  type WalkTarget,
} from './tuiChoices.ts'

export interface Io {
  /** 讀不到或認不出來回 `null`。 */
  read: () => Promise<TuiChoiceMenu | null>
  send: (keys: string[]) => Promise<void>
  wait: (ms: number) => Promise<void>
  /** `POST /bots/{id}/text`（`enter: false`）；`Type something.` 游標停上去直接貼，不必先 Enter（2026-09-13 真機）。 */
  paste: (text: string) => Promise<void>
}

export interface DraftPage {
  tab: number
  label: string
  question: string | null
  /** 讀到當下的選項與勾選狀態。 */
  choices: TuiChoiceMenu['choices']
  multi: boolean
  /** 清單裡有沒編號的 `Submit` 列。 */
  hasSubmitRow: boolean
  review: TuiChoiceMenu['review']
  isSubmit: boolean
}

export interface Draft {
  pages: DraftPage[]
  /** 預載開始（也是結束）的分頁。 */
  startTab: number
  tabs: TuiChoiceMenu['tabs']
}

const SETTLE_TRIES = 6
const SETTLE_STEP = 120

/** 一頁的問卷照舊走即時模式。 */
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
    isSubmit: menu.review.length > 0 || menu.choices.some((c) => /^submit answers$/i.test(c.title)),
  }
}

/** 走一格分頁並確認畫面換了。先 ←／→（別處無副作用），沒反應才 `tab`／`shift+tab`；走不動回 `null`。 */
export async function moveTab(io: Io, dir: 1 | -1, from: TuiChoiceMenu): Promise<TuiChoiceMenu | null> {
  for (const key of dir > 0 ? ['right', 'tab'] : ['left', 'shift+tab']) {
    await io.send([key])
    for (let i = 0; i < SETTLE_TRIES; i++) {
      await io.wait(SETTLE_STEP)
      const now = await io.read()
      if (now && (now.question !== from.question || now.review.length !== from.review.length)) return now
    }
  }
  return null
}

/**
 * 左到頭、右到尾、再走回起點，每頁讀一次。走不動就回 `null`（不留半份草稿），呼叫端退回即時模式。
 * 副作用：真終端的分頁會跳來跳去，但不送任何會改答案的鍵。
 */
export async function preload(
  io: Io,
  start: TuiChoiceMenu,
  onProgress?: (done: number, total: number) => void,
): Promise<Draft | null> {
  const tabs = start.tabs
  const n = tabs.length
  const startTab = start.tabAt
  if (n < MIN_TABS || startTab === null) return null

  const pages: (DraftPage | undefined)[] = new Array<DraftPage | undefined>(n)
  let cur: TuiChoiceMenu = start
  let at = startTab
  let done = 0
  const note = () => onProgress?.(++done, n)

  pages[at] = pageOf(cur, at, tabs[at].label)
  note()

  for (const dir of [-1, 1] as const) {
    // 第二圈經過讀過的頁照樣重讀：便宜，順便確認畫面沒變。
    const stop = dir < 0 ? 0 : n - 1
    while (at !== stop) {
      const next = await moveTab(io, dir, cur)
      if (!next) return null
      at += dir
      cur = next
      const had = pages[at]
      pages[at] = pageOf(cur, at, tabs[at]?.label ?? `第 ${at + 1} 題`)
      if (!had) note()
    }
  }

  while (at > startTab) {
    const next = await moveTab(io, -1, cur)
    if (!next) return null
    at -= 1
    cur = next
  }

  const full = pages.filter((p): p is DraftPage => Boolean(p))
  return full.length === n ? { pages: full, startTab, tabs } : null
}

/** 預設＝讀到當下的勾選；單選預設全沒選。 */
export function wantOf(page: DraftPage): boolean[] {
  if (!page.multi) return page.choices.map(() => false)
  return page.choices.map((c) => c.checked === true)
}

/** 沒點過是 `-1`。 */
export function radioPick(want: boolean[] | undefined): number {
  if (!want) return -1
  const i = want.findIndex(Boolean)
  return i
}

export function pageNeedsCommit(page: DraftPage, want: boolean[] | undefined): boolean {
  if (page.isSubmit) return false
  if (!page.multi) return radioPick(want) >= 0
  return togglesFor(page.choices, want ?? wantOf(page)).length > 0
}

/** 要翻轉的 index。複選是 toggle，一定比差集、不照點擊次數送；沒方框的列是動作，不算。 */
export function togglesFor(now: TuiChoiceMenu['choices'], want: boolean[]): number[] {
  const out: number[] = []
  now.forEach((c, i) => {
    if (c.checked === null) return
    if (c.checked !== Boolean(want[i])) out.push(i)
  })
  return out
}

/** 從畫面題目反查分頁位置。別用 `tabAt`：它照 ☒ 猜，按過 Submit 會跳但終端還在原地。 */
export function locate(draft: Draft, now: TuiChoiceMenu): number | null {
  const i = draft.pages.findIndex(
    (p) => p.question === now.question && p.choices.length === now.choices.length,
  )
  return i >= 0 ? draft.pages[i].tab : null
}

export function samePage(page: DraftPage, now: TuiChoiceMenu): boolean {
  return (
    page.question === now.question &&
    page.choices.length === now.choices.length &&
    page.choices.every((c, i) => c.title === now.choices[i].title && c.number === now.choices[i].number)
  )
}

/** 走一步驗一步：清單裡可能有認不得但游標走得到的列。 */
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

export interface CommitResult {
  ok: boolean
  error?: string
  /** 失敗在 `pages` 的哪個 index。 */
  at?: number
}

/**
 * 逐頁：走到 → 比對同一份 → 送差集 → 重讀對帳 → 有 Submit 列就按 → 最後按 `Submit answers`。
 * 任一步對不上就停。Enter 不落在核取方塊上：翻轉只用 `space`，不行才數字鍵。
 */
export async function commit(
  io: Io,
  draft: Draft,
  want: boolean[][],
  onProgress?: (done: number, total: number) => void,
  custom: string[] = [],
): Promise<CommitResult> {
  const todo = draft.pages.map((p, i) => ({ p, i })).filter(({ p, i }) => pageNeedsCommit(p, want[i]))

  for (const { p, i } of todo) {
    // 複選頁要把 `Type something` 勾起來：字送不出去，只會交出一個空白的自訂答案（review3 c1 M8）。
    if (p.multi && (want[i] ?? []).some((on, j) => on && !p.choices[j]?.checked && isTypeSomething(p.choices[j] ?? { title: '' }))) {
      return { ok: false, error: `第 ${p.tab + 1} 題：${MULTI_TYPE_HINT}什麼都沒送出。`, at: i }
    }
    const idx = radioPick(want[i])
    if (!p.multi && idx >= 0 && isTypeSomething(p.choices[idx] ?? { title: '' })) {
      if (!(custom[p.tab] ?? '').trim()) {
        return {
          ok: false,
          error: `第 ${p.tab + 1} 題選了「Type something.」但沒有打字。請打一段或改選別項。`,
          at: i,
        }
      }
    }
  }

  let cur = await io.read()
  if (!cur) return { ok: false, error: '讀不到畫面，什麼都沒送出。' }
  if (cur.tabs.length !== draft.tabs.length) {
    return { ok: false, error: '畫面已經變了（分頁數不一樣），整批都沒送出，請重新讀取。' }
  }
  let at = locate(draft, cur) ?? draft.startTab

  let done = 0
  const total = todo.length + 1

  for (const { p, i } of todo) {
    while (at !== p.tab) {
      const dir = p.tab > at ? 1 : -1
      const next = await moveTab(io, dir, cur)
      if (!next) return { ok: false, error: `走不到第 ${p.tab + 1} 題（${p.label}），後面的都沒送出。`, at: i }
      cur = next
      at = locate(draft, cur) ?? at + dir
    }
    if (!samePage(p, cur)) {
      return { ok: false, error: `第 ${p.tab + 1} 題（${p.label}）的選項跟讀進來時不一樣，沒有送出。`, at: i }
    }

    // 單選：走到再 Enter，畫面可能自己跳下一題。`Type something.` 先貼字、確認出現再 Enter。
    if (!p.multi) {
      const idx = radioPick(want[i])
      if (idx < 0) continue
      const landed = await walkTo(io, idx, p)
      if (!landed) return { ok: false, error: `第 ${p.tab + 1} 題的游標走不到第 ${idx + 1} 項，停在這裡。`, at: i }
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
          return { ok: false, error: `第 ${p.tab + 1} 題的自訂文字沒有出現在畫面上，沒有按 Enter。`, at: i }
        }
      }
      await io.send(['enter'])
      await io.wait(SETTLE_STEP * 3)
      const now = await io.read()
      if (!now) return { ok: false, error: `第 ${p.tab + 1} 題送出後讀不到畫面，請重新讀取。`, at: i }
      cur = now
      at = locate(draft, now) ?? at
      onProgress?.(++done, total)
      continue
    }

    for (const idx of togglesFor(cur.choices, want[i])) {
      const landed = await walkTo(io, idx, p)
      if (!landed) return { ok: false, error: `第 ${p.tab + 1} 題的游標走不到第 ${idx + 1} 項，停在這裡。`, at: i }
      const before = landed.choices[idx].checked
      const flipped = (now: TuiChoiceMenu | null) => Boolean(now && now.choices[idx]?.checked !== before)
      if (await pressUntil(io, 'space', flipped)) continue
      const n = p.choices[idx].number
      if (n <= 9 && (await pressUntil(io, String(n), flipped))) continue
      return { ok: false, error: `第 ${p.tab + 1} 題的第 ${n} 項按不動（space 與數字鍵都沒反應），停在這裡。`, at: i }
    }

    const after = await io.read()
    if (!after || !samePage(p, after)) {
      return { ok: false, error: `第 ${p.tab + 1} 題送完之後畫面對不上，後面的都沒送出。`, at: i }
    }
    if (togglesFor(after.choices, want[i]).length) {
      return { ok: false, error: `第 ${p.tab + 1} 題的勾選沒有全部生效，後面的都沒送出。`, at: i }
    }
    cur = after
    onProgress?.(++done, total)

    // 多選要按這頁的 Submit 列才算答完。
    if (p.hasSubmitRow) {
      const landed = await walkTo(io, 'submit', p)
      if (!landed?.submit?.current) {
        return { ok: false, error: `第 ${p.tab + 1} 題的游標沒有停在 Submit 上，沒有按下去。`, at: i }
      }
      await io.send(['enter'])
      await io.wait(SETTLE_STEP * 3)
      const now = await io.read()
      if (!now) return { ok: false, error: `第 ${p.tab + 1} 題送出後讀不到畫面，請重新讀取。`, at: i }
      cur = now
      at = locate(draft, now) ?? at
    }
  }

  const submitTab = draft.pages.findIndex((p) => p.isSubmit)
  const target = submitTab >= 0 ? draft.pages[submitTab].tab : draft.tabs.findIndex((t) => t.submit)
  if (target < 0) return { ok: false, error: '找不到送出頁，勾選已經送出去了，最後一步請自己按。' }
  while (at !== target) {
    const dir = target > at ? 1 : -1
    const next = await moveTab(io, dir, cur)
    if (!next) return { ok: false, error: '走不到送出頁，勾選已經送出去了，最後一步請自己按。' }
    cur = next
    at = locate(draft, cur) ?? at + dir
  }
  const idx = cur.choices.findIndex((c) => /^submit answers$/i.test(c.title))
  if (idx < 0) return { ok: false, error: '送出頁上找不到「Submit answers」，最後一步請自己按。' }
  const page = pageOf(cur, target, draft.tabs[target]?.label ?? 'Submit')
  const landed = await walkTo(io, idx, page)
  if (!landed || landed.cursor !== idx) return { ok: false, error: '游標沒有停在「Submit answers」上，沒有按下去。' }
  await io.send(['enter'])
  onProgress?.(++done, total)
  return { ok: true }
}

export function parse(text: string | null | undefined): TuiChoiceMenu | null {
  return parseChoiceMenu(text)
}

/** 值得走預載：≥2 分頁且猜得出目前那格。 */
export function isSurvey(menu: TuiChoiceMenu): boolean {
  return menu.tabs.length >= MIN_TABS && menu.tabAt !== null
}
