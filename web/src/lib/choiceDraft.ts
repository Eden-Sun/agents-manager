/**
 * 多分頁問卷的「先讀完、離線作答、最後一次送出」（2026-09-12 第六輪，使用者指定的做法：
 * 「一開始就先預載入所有的分頁，而不是每一次連動，最後送出前再確認就好」）。
 *
 * 之前每點一下就要送鍵進終端、再重讀畫面對帳，一次點擊 4～6 個 HTTP、1.5～2.5 秒。改成：
 *
 * 1. [`preload`]：用 ←／→ 把每個分頁走一輪，每停一格讀一次畫面，**走完回到原本那一頁**。
 *    這段只送導覽鍵，一顆 `space`／數字／`enter` 都不送——預載不能改到任何答案。
 * 2. 作答：全部只改本地 state，完全不碰終端（點擊是即時的）。
 * 3. [`commit`]：按送出才動終端。每一頁先比對「還是同一份選單嗎」，再算**差集**只送需要翻轉
 *    的那幾顆（複選是 toggle，重送會翻回去，所以一定要比差集而不是照點過幾次送），送完再讀
 *    一次確認該頁狀態符合預期；任何一步對不上就停下、不送後面的鍵。
 *
 * 這裡只有純邏輯：所有 I/O 走 [`Io`] 注入，測試拿一個假終端跑完整條流程
 * （`choiceDraft.test.ts`），不必對真的 bot 按鍵。
 */
import {
  customAnswerShown,
  isTypeSomething,
  keysToMove,
  parseChoiceMenu,
  type TuiChoiceMenu,
  type WalkTarget,
} from './tuiChoices.ts'

/** 跟終端打交道的三件事。元件傳真的進來，測試傳假的。 */
export interface Io {
  /** 讀一次畫面並解析；讀不到或認不出來回 `null`。 */
  read: () => Promise<TuiChoiceMenu | null>
  send: (keys: string[]) => Promise<void>
  wait: (ms: number) => Promise<void>
  /**
   * 貼一段字（`POST /bots/{id}/text`，`enter: false`）。
   * `Type something.` 那列游標停上去之後直接貼，不必先 Enter（2026-09-13 真機）。
   */
  paste: (text: string) => Promise<void>
}

/** 預載到的一頁。 */
export interface DraftPage {
  /** 分頁列上的第幾格。 */
  tab: number
  label: string
  question: string | null
  /** 讀到當下的選項（含當時的勾選狀態）。 */
  choices: TuiChoiceMenu['choices']
  multi: boolean
  /** 這一頁的清單裡有沒有那列沒有編號的 `Submit`。 */
  hasSubmitRow: boolean
  /** 送出頁才有的「每題 → 目前答案」。 */
  review: TuiChoiceMenu['review']
  /** 這一頁是不是整份問卷的送出頁。 */
  isSubmit: boolean
}

export interface Draft {
  pages: DraftPage[]
  /** 預載開始（也是結束）時停在第幾格。 */
  startTab: number
  tabs: TuiChoiceMenu['tabs']
}

/** 每送一顆鍵之後最多等多久看畫面變化。 */
const SETTLE_TRIES = 6
const SETTLE_STEP = 120

/** 預載至少要有兩個分頁才划算；一頁的問卷照舊走即時模式。 */
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

/**
 * 走一格分頁並確認畫面真的換了。
 *
 * 先送 ←／→（分頁列兩端畫的就是這兩顆，別處沒有副作用），沒反應才退回 `tab`／`shift+tab`
 * （腳註寫的是 `Tab/Arrow keys`，兩種都可能）。回傳走完那一格的畫面，走不動回 `null`。
 */
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
 * 把每一個分頁讀一輪，最後回到原本那一頁。
 *
 * 路線是「先往左走到第一頁，再一路往右走到最後一頁，再往左走回起點」——每一步都驗畫面真的
 * 換了，走不動就整個放棄（回 `null`），呼叫端退回即時模式，不要留一份讀了一半的草稿。
 *
 * 副作用說在前面：真的終端游標與分頁**會跳來跳去**，同時盯著終端分頁的人看得到。走完一定
 * 回到起點，而且全程不送任何會改到答案的鍵。
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
    // 先往左收到 0，再從 0 往右收到 n-1。第二圈會經過已經讀過的那幾頁，照樣重讀（便宜，
    // 而且順便確認畫面沒有在中途變掉）。
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

  // 回到起點。
  while (at > startTab) {
    const next = await moveTab(io, -1, cur)
    if (!next) return null
    at -= 1
    cur = next
  }

  const full = pages.filter((p): p is DraftPage => Boolean(p))
  return full.length === n ? { pages: full, startTab, tabs } : null
}

/** 這一頁預設的「想要的狀態」＝讀到當下的勾選狀態。單選沒有方框，預設全沒選。 */
export function wantOf(page: DraftPage): boolean[] {
  if (!page.multi) return page.choices.map(() => false)
  return page.choices.map((c) => c.checked === true)
}

/** 單選頁使用者點了哪一項（最多一個 `true`）。沒點過是 `-1`。 */
export function radioPick(want: boolean[] | undefined): number {
  if (!want) return -1
  const i = want.findIndex(Boolean)
  return i
}

/** 這一頁送出時要不要動終端。單選看有沒有點過；複選看差集。 */
export function pageNeedsCommit(page: DraftPage, want: boolean[] | undefined): boolean {
  if (page.isSubmit) return false
  if (!page.multi) return radioPick(want) >= 0
  return togglesFor(page.choices, want ?? wantOf(page)).length > 0
}

/**
 * 目前狀態 → 想要狀態，要翻轉哪幾個 index。
 *
 * 複選是 toggle：同一顆送兩次會翻回去，所以**一定是比差集**，不是照使用者點過幾次送。
 * 沒有核取方塊的列（`Type something` / `Chat about this`）不在這裡面——那是動作不是勾選。
 */
export function togglesFor(now: TuiChoiceMenu['choices'], want: boolean[]): number[] {
  const out: number[] = []
  now.forEach((c, i) => {
    if (c.checked === null) return
    if (c.checked !== Boolean(want[i])) out.push(i)
  })
  return out
}

/**
 * 這張畫面是草稿裡的第幾格分頁。
 *
 * **不要用 `tabAt` 當位置**：那是照 ☒ 猜的「該答哪一題」，按過某頁的 Submit 之後它會跳，
 * 但終端其實還停在原地。位置一律從畫面上的題目反查回來，每一步都重新定位，走錯也會自己修正。
 */
export function locate(draft: Draft, now: TuiChoiceMenu): number | null {
  const i = draft.pages.findIndex(
    (p) => p.question === now.question && p.choices.length === now.choices.length,
  )
  return i >= 0 ? draft.pages[i].tab : null
}

/** 重讀到的畫面還是草稿裡那一頁嗎（問題與每個選項的字串都要對得上）。 */
export function samePage(page: DraftPage, now: TuiChoiceMenu): boolean {
  return (
    page.question === now.question &&
    page.choices.length === now.choices.length &&
    page.choices.every((c, i) => c.title === now.choices[i].title && c.number === now.choices[i].number)
  )
}

/** 走到某一列並確認游標真的停在那裡。清單裡可能有我們認不得但游標走得到的列，所以走一步驗一步。 */
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

/** 送一顆鍵，等到 `done` 成立為止。 */
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
  /** 失敗時給使用者看的一句話。 */
  error?: string
  /** 做到第幾頁（`pages` 的 index），失敗時用來說「第 N 題沒對上」。 */
  at?: number
}

/**
 * 把草稿一次送進終端。
 *
 * 順序：走到那一頁 → 比對還是同一份 → 只送差集那幾顆 → 再讀一次確認整頁符合預期 →
 * 這一頁有 `Submit` 列就按它（確認游標真的停在上面才 Enter）→ 全部做完才走到送出頁按
 * `Submit answers`。任何一步對不上就**停在那裡**，不繼續送後面的鍵。
 *
 * 一顆 Enter 都不會落在核取方塊上：翻轉只用 `space`，不行再用數字鍵，兩個都沒反應就算失敗。
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
    // 走到那一頁
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

    // 單選：沒有核取方塊、沒有這一頁的 Submit 列。走到那一項再 Enter，畫面可能自己跳下一題。
    // `Type something.`：游標停上去之後直接貼字（不必先 Enter），標題被取代後再 Enter。
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

    // 複選：只送差集
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

    // 整頁對帳
    const after = await io.read()
    if (!after || !samePage(p, after)) {
      return { ok: false, error: `第 ${p.tab + 1} 題送完之後畫面對不上，後面的都沒送出。`, at: i }
    }
    if (togglesFor(after.choices, want[i]).length) {
      return { ok: false, error: `第 ${p.tab + 1} 題的勾選沒有全部生效，後面的都沒送出。`, at: i }
    }
    cur = after
    onProgress?.(++done, total)

    // 這一頁自己的 Submit 列（多選問卷是按它才算答完這一題）。
    if (p.hasSubmitRow) {
      const landed = await walkTo(io, 'submit', p)
      if (!landed?.submit?.current) {
        return { ok: false, error: `第 ${p.tab + 1} 題的游標沒有停在 Submit 上，沒有按下去。`, at: i }
      }
      await io.send(['enter'])
      await io.wait(SETTLE_STEP * 3)
      const now = await io.read()
      if (!now) return { ok: false, error: `第 ${p.tab + 1} 題送出後讀不到畫面，請重新讀取。`, at: i }
      // 按完可能自己跳到下一頁；位置重新從畫面反查回來，不要用 `tabAt` 猜。
      cur = now
      at = locate(draft, now) ?? at
    }
  }

  // 最後：走到送出頁按 `Submit answers`。
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

/** 給元件用的小工具：把一張快照的文字直接解析成菜單（測試也用得到）。 */
export function parse(text: string | null | undefined): TuiChoiceMenu | null {
  return parseChoiceMenu(text)
}

/** 這份選單值不值得走預載（兩個分頁以上，而且猜得出現在停在哪一格）。 */
export function isSurvey(menu: TuiChoiceMenu): boolean {
  return menu.tabs.length >= MIN_TABS && menu.tabAt !== null
}
