/**
 * agent 停在編號選單上時，把選單畫成**可以直接點**的清單（2026-09-12 使用者回報）。
 *
 * 原本要在手機上選第 4 項，得在下面那排鍵按三次 ↓ 再按 Enter——而且選項到底寫什麼，得去戳
 * 終端快照裡 11px 的字，長一點的說明還被終端寬度截掉。這裡把 `parseChoiceMenu` 讀出來的
 * 問題、選項、被折掉的說明攤成一列一列 44px 的按鈕，點一下就答完。
 *
 * 第三輪（多分頁 ＋ 多選）多了三件事：上面那條分頁列（一次問好幾題，☑ 是答過的）、選項是
 * 核取方塊（點一下是**切換勾選**，不是送出）、最後要走到 `Submit` 才真的交卷。
 *
 * 第四輪：**說明預設收起來**，一列就是「編號＋標題」一行，六個選項一屏看得完；要看說明按那顆
 * ▸。收合狀態下捲動與 ↑／↓ 照舊能用，而且游標換到哪一項就把那一項捲進視野；展開再收起來時
 * 捲動位置不會跳（收合前記下那一列的位置，重畫完補回去）。
 *
 * 認不出選單就什麼都不畫（回 `null`），畫面照舊退回終端快照＋按鍵面板——按鍵是直接送進別人
 * 終端的，寧可少一個捷徑，也不要在認錯的畫面上替使用者答題。
 */
import { useLayoutEffect, useRef, useState } from 'react'
import { usePaneKeys } from '../hooks/usePaneKeys'
import {
  keysToMove,
  keysToSelect,
  parseChoiceMenu,
  sameChoices,
  type TuiChoiceMenu,
  type WalkTarget,
} from '../lib/tuiChoices'
import { useStore } from '../store/store'
import './blockedChoices.css'

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/**
 * 這個元素是被誰捲著的。
 *
 * 全畫面視窗裡是 `.blocked-modal-body`，對話上方那條面板裡是聊天區自己。找不到就回 `null`
 * ——那代表整頁在捲，收合造成的位移瀏覽器自己的 scroll anchoring 會處理。
 */
function scroller(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflowY
    if ((o === 'auto' || o === 'scroll') && p.scrollHeight > p.clientHeight + 1) return p
  }
  return null
}

/** 送出一顆鍵之後等畫面反應：每 `STEP` 看一次，最多看 `TRIES` 次。 */
const STEP = 260
const TRIES = 3

/** 走到目標列最多修正幾次（清單裡可能還有我們認不得、但游標走得到的列）。 */
const WALK_TRIES = 3

type Busy = { kind: 'choice' | 'submit' | 'tab'; key: string } | null

export function BlockedChoices({
  botId,
  menu,
  onAnswered,
}: {
  botId: string
  menu: TuiChoiceMenu
  /** 送出後叫一次，讓外面那張終端快照立刻重抓，不必等下一秒的輪詢。 */
  onAnswered?: () => void
}) {
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const [busy, setBusy] = useState<Busy>(null)
  const [note, setNote] = useState<string | null>(null)
  /**
   * 「現在在第幾個分頁」**讀不出來**：終端上那是用顏色標的，快照只剩純文字。先猜第一個還沒
   * 答（☐）的那一格——claude 是照順序帶著人走的——之後每次自己切分頁就記下切到哪，UI 上也
   * 寫明是推測。猜錯的代價只是跳到別題（切分頁不會答題），再點一下就好。
   */
  const [tabAt, setTabAt] = useState<number | null>(null)
  const atTab = tabAt ?? menu.tabAt

  /**
   * 展開說明的那幾項（2026-09-12 第四輪）。
   *
   * 預設全部收起來：實拍那張六個選項、每項三到五行說明，攤開要滑三四屏才看得完一組選項。
   * 換一題就忘掉——那是另一組選項了。
   */
  const [open, setOpen] = useState<number[]>([])
  const [openFor, setOpenFor] = useState(menu.question)
  if (openFor !== menu.question) {
    setOpenFor(menu.question)
    setOpen([])
  }

  const listRef = useRef<HTMLOListElement>(null)
  /** 收合前記下的「那一列離捲動容器上緣多遠」，重畫完補回去，畫面才不會跳。 */
  const anchor = useRef<{ el: HTMLElement; top: number } | null>(null)
  const lastCursor = useRef(menu.cursor)

  useLayoutEffect(() => {
    // a) 收合／展開之後把捲動位置補回去。
    const a = anchor.current
    anchor.current = null
    if (a) {
      const box = scroller(a.el)
      const now = a.el.getBoundingClientRect().top
      if (box) box.scrollTop += now - a.top
      return
    }
    // b) 標題被截尾的那幾列也要有 ▸（沒有說明、但字放不下時，全文得有地方看）。
    //
    // 量完直接寫 `data-wide` 而不是進 state：這是純量測結果，走 state 會多一輪 render，而
    // React 不管 `data-*`，重畫也不會把它洗掉。展開中的那列不量——它的標題本來就折行了。
    listRef.current?.querySelectorAll<HTMLElement>('.bc-row').forEach((li) => {
      if (li.querySelector('.bc-open')) return
      const t = li.querySelector('.bc-title')
      if (t && t.scrollWidth > t.clientWidth + 1) li.dataset.wide = '1'
      else delete li.dataset.wide
    })
  })

  // 游標換到哪一項就把那一項捲進視野——收合之後一屏多半看得完，但選項多的時候仍會捲出去，
  // 而 ↑／↓ 是直接送進終端的，畫面不跟上就等於在盲按。只在游標真的變了的時候動。
  useLayoutEffect(() => {
    if (lastCursor.current === menu.cursor) return
    lastCursor.current = menu.cursor
    listRef.current
      ?.querySelectorAll<HTMLElement>('.bc-item')
      [menu.cursor]?.scrollIntoView({ block: 'nearest' })
  }, [menu.cursor])

  const toggleDetail = (i: number, el: HTMLElement) => {
    const box = scroller(el)
    if (box) anchor.current = { el, top: el.getBoundingClientRect().top }
    setOpen((v) => (v.includes(i) ? v.filter((n) => n !== i) : [...v, i]))
  }

  const read = async (): Promise<TuiChoiceMenu | null> => {
    try {
      return parseChoiceMenu((await readTerminal(botId, 'visible', 200)).text)
    } catch {
      return null
    }
  }

  /** 畫面還是同一份選單嗎。不是就什麼都不送——走幾格是相對的，換了題就會按到別的東西。 */
  const stillHere = (now: TuiChoiceMenu | null): now is TuiChoiceMenu => Boolean(now && sameChoices(now, menu))

  /**
   * 把游標走到某一列：**每一步都用當下重讀的畫面重算**，走完再確認真的停在那裡。
   *
   * 清單裡除了編號選項還可能有我們認不得、但游標走得到的列（真機那個 `Submit` 就是），只算
   * 一次會差一格。走一步、看一眼、再修正，就不必猜那種列到底有幾個。
   */
  const walkTo = async (target: WalkTarget): Promise<TuiChoiceMenu | null> => {
    for (let i = 0; i < WALK_TRIES; i++) {
      const now = await read()
      if (!stillHere(now)) return null
      const at: WalkTarget | null = now.submit?.current ? 'submit' : now.cursor >= 0 ? now.cursor : null
      if (at === target) return now
      // 游標落在我們認不得的列上：先往上挪一格回到認得的位置，下一圈重算。
      const keys = at === null ? ['up'] : (keysToMove(now, target) ?? [])
      if (!keys.length) return null
      await sendKeys(botId, keys)
      await sleep(STEP)
    }
    return null
  }

  /** 送一顆鍵，然後等畫面出現預期的變化。`done` 回 true 就算成功。 */
  const pressUntil = async (key: string, done: (now: TuiChoiceMenu) => boolean): Promise<boolean> => {
    await sendKeys(botId, [key])
    for (let i = 0; i < TRIES; i++) {
      await sleep(STEP)
      const now = await read()
      // 選單整個不見／換了一題＝這顆鍵被收下了（單選就是這樣結束的）。
      if (!now || !sameChoices(now, menu)) return true
      if (done(now)) return true
    }
    return false
  }

  const run = async (b: NonNullable<Busy>, fn: () => Promise<string | null>) => {
    if (busy) return
    setBusy(b)
    setNote(null)
    try {
      setNote(await fn())
    } finally {
      setBusy(null)
      onAnswered?.()
    }
  }

  /**
   * 單選：走過去再 Enter，一批送完（2026-09-12 第一輪實測過的路徑）。送出**前**先重讀一次，
   * 用當下的游標算——手上這份快照最多是一秒前的。
   */
  const pickOne = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const now = await read()
      if (!stillHere(now)) return '畫面在這中間換過了，剛剛那一下沒有送出去——上面是重讀後的選單，請再點一次。'
      await sendKeys(botId, keysToSelect(now, i))
      return null
    })

  /**
   * 多選：切換勾選。**一次都不按 Enter**——Enter 在這種選單上可能是「交卷」，猜錯就替使用者
   * 答了一整題。先走到那一列，再試 space；沒反應才試數字鍵；兩個都沒反應就照實說，按鍵面板
   * 還在下面。成功與否一律看畫面上的 `[ ]` 有沒有變成 `[x]`，不靠猜。
   */
  const toggle = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const landed = await walkTo(i)
      if (!landed) return '游標走不到這一項（畫面可能換了），什麼都沒送出。'
      const before = landed.choices[i].checked
      const flipped = (now: TuiChoiceMenu) => now.choices[i]?.checked !== before
      if (await pressUntil('space', flipped)) return null
      const n = menu.choices[i].number
      if (n <= 9 && (await pressUntil(String(n), flipped))) return null
      return '這個選單不吃點選（space 與數字鍵都沒反應），請用下面的按鍵。'
    })

  /** 沒有核取方塊的列（`Type something` / `Chat about this`）是動作，不是勾選。 */
  const activate = (i: number) => (menu.multi && menu.choices[i].checked !== null ? toggle(i) : pickOne(i))

  /** 送出這一題：走到清單裡那列 `Submit`，**確認游標真的停在它上面**，才按 Enter。 */
  const submit = () =>
    run({ kind: 'submit', key: 'submit' }, async () => {
      const landed = await walkTo('submit')
      if (!landed?.submit?.current) return '游標沒有停在 Submit 上，沒有送出。'
      await sendKeys(botId, ['enter'])
      return null
    })

  /**
   * 走 `n` 格分頁並確認畫面真的換了。
   *
   * 先送 ←／→——分頁列兩端畫的就是這兩顆箭頭，而且它們在別的地方不會有副作用；
   * 沒反應才退回 tab／shift+tab（腳註寫的是 `Tab/Arrow keys`，兩種都可能）。
   * **一顆都不碰 space／數字／Enter**：換頁不該改到任何答案。
   */
  const moveTabs = async (delta: number): Promise<boolean> => {
    const before = await read()
    if (!before) return false
    const n = Math.abs(delta)
    for (const key of delta > 0 ? ['right', 'tab'] : ['left', 'shift+tab']) {
      await sendKeys(
        botId,
        Array.from({ length: n }, () => key),
      )
      for (let i = 0; i < TRIES; i++) {
        await sleep(STEP)
        const now = await read()
        if (!now || now.question !== before.question || now.tabAt !== before.tabAt) return true
      }
    }
    return false
  }

  /** 點某一個分頁：走過去，換到了才把「現在在哪一題」記下來。 */
  const goTab = (to: number) =>
    run({ kind: 'tab', key: String(to) }, async () => {
      if (atTab === null) return '看不出現在在第幾題，請用左右那兩顆一格一格走。'
      if (to === atTab) return null
      if (await moveTabs(to - atTab)) {
        setTabAt(to)
        return null
      }
      return '分頁沒有換——現在可能不在我猜的那一題上，請用左右那兩顆一格一格走。'
    })

  const step = (dir: 1 | -1) =>
    run({ kind: 'tab', key: dir > 0 ? 'next' : 'prev' }, async () => {
      if (await moveTabs(dir)) {
        if (atTab !== null) setTabAt(Math.min(Math.max(atTab + dir, 0), menu.tabs.length - 1))
        return null
      }
      return '分頁沒有換。'
    })

  /** review 那一列對應到第幾個分頁（送出頁不算一題）。 */
  const reviewTab = (i: number) => menu.tabs.findIndex((t, k) => !t.submit && menu.tabs.slice(0, k).filter((x) => !x.submit).length === i)

  return (
    <div className="blocked-choices">
      <div className="bc-top">
        {menu.tabs.length ? (
          <div className="bc-tabs" role="group" aria-label="這份問卷的各題">
            <button
              type="button"
              className="bc-step"
              title="上一題（送 shift+tab）"
              disabled={Boolean(busy)}
              onClick={() => void step(-1)}
            >
              ←
            </button>
            {menu.tabs.map((t, i) => (
              <button
                key={t.label}
                type="button"
                className={`bc-tab${i === atTab ? ' bc-tab-at' : ''}${t.done ? ' bc-tab-done' : ''}`}
                aria-current={i === atTab ? 'true' : undefined}
                disabled={Boolean(busy)}
                title={
                  i === atTab
                    ? '應該就是現在這一題（終端只用顏色標，快照讀不出來，這是推測）'
                    : `切到「${t.label}」${t.done ? '（已經答過）' : ''}`
                }
                onClick={() => void goTab(i)}
              >
                <span aria-hidden="true">{t.submit ? '✔' : t.done ? '☑' : '☐'}</span>
                {t.label}
              </button>
            ))}
            <button
              type="button"
              className="bc-step"
              title="下一題（送 tab）"
              disabled={Boolean(busy)}
              onClick={() => void step(1)}
            >
              →
            </button>
          </div>
        ) : null}
        {menu.question ? <p className="bc-question">{menu.question}</p> : null}
      </div>

      {/* review／confirm 頁中段那段「每題 → 目前答案」。使用者在這一頁最想確認的就是自己答了
          什麼，而那段原本只存在收起來的終端原文裡；點一題就跳回那個分頁去改（2026-09-12 第七輪）。 */}
      {menu.review.length ? (
        <ul className="bc-review">
          {menu.review.map((r, i) => (
            <li key={r.question}>
              <button
                type="button"
                className="bc-rev"
                disabled={Boolean(busy) || reviewTab(i) < 0}
                title={`回到「${menu.tabs[reviewTab(i)]?.label ?? '這一題'}」改答案`}
                onClick={() => void goTab(reviewTab(i))}
              >
                <span className="bc-rev-q">{r.question}</span>
                <span className="bc-rev-a">{r.answer || '（還沒作答）'}</span>
              </button>
            </li>
          ))}
        </ul>
      ) : null}

      <ol className={`bc-list${menu.multi ? ' bc-list-multi' : ''}`}>
        {menu.choices.map((c, i) => {
          const shown = open.includes(i)
          return (
          <li key={`${c.number}-${c.title}`} className={`bc-row${c.detail ? ' bc-row-more' : ''}`}>
            <button
              type="button"
              className={`bc-item${c.current ? ' bc-current' : ''}${c.checked ? ' bc-checked' : ''}${
                shown ? ' bc-open' : ''
              }`}
              disabled={Boolean(busy)}
              aria-current={c.current ? 'true' : undefined}
              aria-pressed={c.checked === null ? undefined : c.checked}
              title={
                c.checked === null
                  ? '選這一項（游標移過去再按 Enter）'
                  : c.checked
                    ? '取消勾選（游標移過去再按 space）'
                    : '勾選這一項（游標移過去再按 space），勾完按下面的「送出」'
              }
              onClick={() => void activate(i)}
            >
              <span className="bc-num" aria-hidden="true">
                {c.number}
              </span>
              {/* 多選清單裡沒有方框的列（`Chat about this`）也要占住那一欄，不然本文會比
                  上面幾列往左凸一截。 */}
              {menu.multi ? (
                <span className="bc-box" aria-hidden="true">
                  {c.checked === null ? '' : c.checked ? '☑' : '☐'}
                </span>
              ) : null}
              <span className="bc-title">{c.title}</span>
              <span className="bc-mark">
                {busy?.kind === 'choice' && busy.key === String(i) ? '送出中…' : c.current ? '游標在此' : ''}
              </span>
              {shown && c.detail ? <span className="bc-detail">{c.detail}</span> : null}
            </button>
            {/* ▸ 要是這一列的**兄弟**不是子元素：`<button>` 裡再包一顆 `<button>` 不合法，而整列
                本身就是「選這一項」那顆按鈕。展開只是看說明，不會送任何鍵。 */}
            <button
                type="button"
                className="bc-chev"
                aria-expanded={shown}
                aria-label={shown ? `收起第 ${c.number} 項的說明` : `看第 ${c.number} 項的說明`}
                title={shown ? '收起說明' : '看這一項的說明'}
                onClick={(e) => toggleDetail(i, e.currentTarget.parentElement as HTMLElement)}
              >
                <span aria-hidden="true">{shown ? '▾' : '▸'}</span>
              </button>
          </li>
          )
        })}
      </ol>

      {menu.submit ? (
        <button type="button" className="bc-submit" disabled={Boolean(busy)} onClick={() => void submit()}>
          {busy?.kind === 'submit' ? '送出中…' : '送出（Submit）'}
        </button>
      ) : null}

      {/* 平常不留任何說明文字（2026-09-12 第二輪回饋：畫面上只要「問題 ＋ 選項 ＋ 送出」）。
          「點一下會送什麼」寫在每顆按鈕的 tooltip；只有真的沒送出去時才需要一句話。 */}
      {note ? (
        <p className="bc-hint bc-stale" role="status">
          {note}
        </p>
      ) : null}
    </div>
  )
}

/**
 * 選單模式底下那一條：一顆 `Esc 取消`，加上把終端原文與整排按鍵收起來的開關。
 *
 * 2026-09-12 第二輪回饋：認出選單之後，`Enter / Esc / y / n / ↑ / ↓ / ctrl+c` 那排鍵、鍵盤
 * 直通勾選框與它那段說明都不該是預設狀態——選單本身就是操作方式。只有 Esc（取消這個問題）
 * 是真的常用，留在選單旁邊；其他的跟終端原文一起收進這顆開關後面。
 */
export function BlockedExtrasBar({
  botId,
  open,
  onToggle,
  onAnswered,
}: {
  botId: string
  open: boolean
  onToggle: () => void
  onAnswered?: () => void
}) {
  const press = usePaneKeys(botId, onAnswered)
  return (
    <div className="bc-bar">
      <button type="button" className="bc-esc" title="送出 Esc：取消這個問題" onClick={() => press(['esc'])}>
        Esc 取消
      </button>
      <button type="button" className="mini-btn bc-more" aria-expanded={open} onClick={onToggle}>
        {open ? '收起終端原文' : '終端原文與更多按鍵'}
      </button>
    </div>
  )
}
