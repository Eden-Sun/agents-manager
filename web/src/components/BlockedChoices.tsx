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
 * 認不出選單就什麼都不畫（回 `null`），畫面照舊退回終端快照＋按鍵面板——按鍵是直接送進別人
 * 終端的，寧可少一個捷徑，也不要在認錯的畫面上替使用者答題。
 */
import { useState } from 'react'
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
  const guessTab = menu.tabs.findIndex((t) => !t.done && !t.submit)
  const atTab = tabAt ?? (guessTab >= 0 ? guessTab : null)

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

  /** 切分頁：Tab 往後、shift+tab 往前。送完看問題有沒有換，換了才把「現在在哪一題」記下來。 */
  const goTab = (to: number) =>
    run({ kind: 'tab', key: String(to) }, async () => {
      if (atTab === null) return '看不出現在在第幾題，請用左右那兩顆一格一格走。'
      const delta = to - atTab
      if (!delta) return null
      const before = await read()
      if (!before) return '讀不到畫面，什麼都沒送出。'
      await sendKeys(
        botId,
        Array.from({ length: Math.abs(delta) }, () => (delta > 0 ? 'tab' : 'shift+tab')),
      )
      for (let i = 0; i < TRIES; i++) {
        await sleep(STEP)
        const now = await read()
        if (!now || now.question !== before.question) {
          setTabAt(to)
          return null
        }
      }
      return '分頁沒有換——現在可能不在我猜的那一題上，請用左右那兩顆一格一格走。'
    })

  const step = (dir: 1 | -1) =>
    run({ kind: 'tab', key: dir > 0 ? 'next' : 'prev' }, async () => {
      await sendKeys(botId, [dir > 0 ? 'tab' : 'shift+tab'])
      if (atTab !== null) setTabAt(Math.min(Math.max(atTab + dir, 0), menu.tabs.length - 1))
      return null
    })

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

      <ol className={`bc-list${menu.multi ? ' bc-list-multi' : ''}`}>
        {menu.choices.map((c, i) => (
          <li key={`${c.number}-${c.title}`}>
            <button
              type="button"
              className={`bc-item${c.current ? ' bc-current' : ''}${c.checked ? ' bc-checked' : ''}`}
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
              {c.detail ? <span className="bc-detail">{c.detail}</span> : null}
            </button>
          </li>
        ))}
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
