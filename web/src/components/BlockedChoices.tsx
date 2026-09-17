/**
 * agent 停在編號選單上時，畫成可直接點的清單（2026-09-12 使用者回報）；多分頁＋多選要走到 Submit 才交卷，說明預設收起。
 * 認不出選單就回 `null` 退回終端快照：按鍵直送別人終端，寧可少捷徑也不在認錯的畫面上替使用者答題。
 */
import { useLayoutEffect, useRef, useState } from 'react'
import { usePaneKeys } from '../hooks/usePaneKeys'
import {
  customAnswerShown,
  isTypeSomething,
  keysToMove,
  keysToSelect,
  MULTI_TYPE_HINT,
  parseChoiceMenu,
  sameChoices,
  typedAnswerHere,
  type TuiChoiceMenu,
  type WalkTarget,
} from '../lib/tuiChoices'
import { useStore } from '../store/store'
import { TypeAnswerField } from './TypeAnswerField'
import './blockedChoices.css'

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/** 最近的捲動容器；`null` ＝整頁在捲，交給瀏覽器 scroll anchoring。 */
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

/** 走到目標列最多修正幾次（清單可能有認不得但游標走得到的列）。 */
const WALK_TRIES = 3

type Busy = { kind: 'choice' | 'submit' | 'tab'; key: string } | null

export function BlockedChoices({
  botId,
  menu,
  onAnswered,
}: {
  botId: string
  menu: TuiChoiceMenu
  onAnswered?: () => void
}) {
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const sendText = useStore((s) => s.sendText)
  const [busy, setBusy] = useState<Busy>(null)
  const [note, setNote] = useState<string | null>(null)
  const [typeText, setTypeText] = useState('')
  const [typeFor, setTypeFor] = useState<number | null>(null)
  // 目前分頁讀不出來（終端用顏色標），先猜第一個 ☐、之後自己記；猜錯只是跳到別題。
  const [tabAt, setTabAt] = useState<number | null>(null)
  const atTab = tabAt ?? menu.tabAt

  // 展開說明的項目，預設全收、換題清空（2026-09-12 第四輪：攤開要滑三四屏）。
  const [open, setOpen] = useState<number[]>([])
  const [openFor, setOpenFor] = useState(menu.question)
  if (openFor !== menu.question) {
    setOpenFor(menu.question)
    setOpen([])
  }

  const listRef = useRef<HTMLOListElement>(null)
  /** 收合前記下列的位置，重畫完補回去以免跳動。 */
  const anchor = useRef<{ el: HTMLElement; top: number } | null>(null)
  const lastCursor = useRef(menu.cursor)

  useLayoutEffect(() => {
    const a = anchor.current
    anchor.current = null
    if (a) {
      const box = scroller(a.el)
      const now = a.el.getBoundingClientRect().top
      if (box) box.scrollTop += now - a.top
      return
    }
    // 標題截尾的列也要有 ▸；直接寫 `data-wide` 不進 state（省一輪 render，React 不會洗掉）。
    listRef.current?.querySelectorAll<HTMLElement>('.bc-row').forEach((li) => {
      if (li.querySelector('.bc-open')) return
      const t = li.querySelector('.bc-title')
      if (t && t.scrollWidth > t.clientWidth + 1) li.dataset.wide = '1'
      else delete li.dataset.wide
    })
  })

  // 游標移動時捲進視野，否則 ↑／↓ 等於盲按。
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

  /** 不是同一份選單就什麼都不送——走幾格是相對的，換題會按到別的東西。 */
  const stillHere = (now: TuiChoiceMenu | null): now is TuiChoiceMenu => Boolean(now && sameChoices(now, menu))

  /** 游標走到某列：每步重讀畫面重算（有認不得的列如 `Submit`，一次算會差一格）。 */
  const walkTo = async (target: WalkTarget): Promise<TuiChoiceMenu | null> => {
    for (let i = 0; i < WALK_TRIES; i++) {
      const now = await read()
      if (!stillHere(now)) return null
      const at: WalkTarget | null = now.submit?.current ? 'submit' : now.cursor >= 0 ? now.cursor : null
      if (at === target) return now
      const keys = at === null ? ['up'] : (keysToMove(now, target) ?? [])
      if (!keys.length) return null
      await sendKeys(botId, keys)
      await sleep(STEP)
    }
    return null
  }

  const pressUntil = async (key: string, done: (now: TuiChoiceMenu) => boolean): Promise<boolean> => {
    await sendKeys(botId, [key])
    for (let i = 0; i < TRIES; i++) {
      await sleep(STEP)
      const now = await read()
      // 選單不見／換題＝鍵被收下了。
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

  /** 單選：送出前重讀、走過去再 Enter 一批送完（2026-09-12 第一輪實測）。 */
  const pickOne = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const now = await read()
      if (!stillHere(now)) return '畫面在這中間換過了，剛剛那一下沒有送出去——上面是重讀後的選單，請再點一次。'
      await sendKeys(botId, keysToSelect(now, i))
      return null
    })

  /** 真機：停在 Type something. 上直接貼字，不必先 Enter；貼完 Enter 才答完。 */
  const sendTyped = (i: number) =>
    run({ kind: 'choice', key: String(i) }, async () => {
      const text = typeText.trim()
      if (!text) return '請先打字，或改選別項。'
      const landed = await walkTo(i)
      if (!landed) return '游標走不到這一項（畫面可能換了），什麼都沒送出。'
      const ok = await sendText(botId, text, false)
      if (!ok) return '文字沒有送出去。'
      let shown = false
      for (let t = 0; t < TRIES; t++) {
        await sleep(STEP)
        const now = await read()
        if (now?.choices[i] && customAnswerShown(now.choices[i], text)) {
          shown = true
          break
        }
      }
      if (!shown) return '自訂文字沒有出現在畫面上，沒有按 Enter。'
      await sendKeys(botId, ['enter'])
      return null
    })

  /** 多選切換：絕不按 Enter（可能是交卷）；試 space 再試數字鍵，成功與否看畫面 `[x]`。 */
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

  const activate = (i: number) => {
    // 複選頁：不貼字、不按 Enter（可能直接交卷），也不勾一個空白的自訂答案（review3 c1 M8）。
    if (menu.multi && isTypeSomething(menu.choices[i]) && !menu.choices[i].checked) {
      setNote(MULTI_TYPE_HINT)
      return
    }
    if (typedAnswerHere(menu, menu.choices[i])) {
      setTypeFor(i)
      setNote(null)
      return
    }
    return menu.multi && menu.choices[i].checked !== null ? toggle(i) : pickOne(i)
  }

  /** 確認游標真的停在 `Submit` 上才按 Enter。 */
  const submit = () =>
    run({ kind: 'submit', key: 'submit' }, async () => {
      const landed = await walkTo('submit')
      if (!landed?.submit?.current) return '游標沒有停在 Submit 上，沒有送出。'
      await sendKeys(botId, ['enter'])
      return null
    })

  /** 換分頁：先 ←／→（無副作用）再退回 tab；絕不碰 space／數字／Enter，換頁不該改答案。 */
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

  /** review 列對應的分頁（送出頁不算一題）。 */
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

      {/* review 頁「每題 → 目前答案」，點一題跳回去改（2026-09-12 第七輪）。 */}
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

      <ol
        className={`bc-list${menu.multi ? ' bc-list-multi' : ' bc-list-radio'}`}
        role={menu.multi ? undefined : 'radiogroup'}
      >
        {menu.choices.map((c, i) => {
          const shown = open.includes(i)
          const typeOpen = typeFor === i && typedAnswerHere(menu, c)
          return (
          <li key={`${c.number}-${c.title}`} className={`bc-row${c.detail ? ' bc-row-more' : ''}`}>
            <button
              type="button"
              role={menu.multi ? undefined : 'radio'}
              className={`bc-item${c.current ? ' bc-current' : ''}${c.checked || typeOpen ? ' bc-checked' : ''}${
                shown ? ' bc-open' : ''
              }`}
              disabled={Boolean(busy)}
              aria-current={c.current ? 'true' : undefined}
              aria-checked={menu.multi ? undefined : c.current}
              aria-pressed={c.checked === null ? undefined : c.checked}
              title={
                menu.multi && isTypeSomething(c) && !c.checked
                  ? MULTI_TYPE_HINT
                  : c.checked === null
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
              {/* 沒方框的列也占住這欄，否則本文往左凸。 */}
              <span className="bc-box" aria-hidden="true">
                {menu.multi
                  ? c.checked === null
                    ? ''
                    : c.checked
                      ? '☑'
                      : '☐'
                  : c.current
                    ? '●'
                    : '○'}
              </span>
              <span className="bc-title">{c.title}</span>
              <span className="bc-mark">
                {busy?.kind === 'choice' && busy.key === String(i) ? '送出中…' : c.current ? '游標在此' : ''}
              </span>
              {shown && c.detail ? <span className="bc-detail">{c.detail}</span> : null}
            </button>
            {/* ▸ 是兄弟不是子元素：button 內不能再包 button。 */}
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
              {typeOpen ? (
                <>
                  <TypeAnswerField value={typeText} onChange={setTypeText} disabled={Boolean(busy)} />
                  <button
                    type="button"
                    className="bc-submit bc-type-send"
                    disabled={Boolean(busy) || !typeText.trim()}
                    onClick={() => void sendTyped(i)}
                  >
                    {busy?.kind === 'choice' && busy.key === String(i) ? '送出中…' : '用這段作答'}
                  </button>
                </>
              ) : null}
          </li>
          )
        })}
      </ol>

      {menu.submit ? (
        <button type="button" className="bc-submit" disabled={Boolean(busy)} onClick={() => void submit()}>
          {busy?.kind === 'submit' ? '送出中…' : '送出（Submit）'}
        </button>
      ) : null}

      {/* 平常不留說明文字，只在沒送出去時提示（2026-09-12 第二輪回饋）。 */}
      {note ? (
        <p className="bc-hint bc-stale" role="status">
          {note}
        </p>
      ) : null}
    </div>
  )
}

/** 選單模式底下：`Esc 取消`＋收起終端原文與按鍵的開關（2026-09-12 第二輪回饋：選單本身就是操作方式）。 */
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
