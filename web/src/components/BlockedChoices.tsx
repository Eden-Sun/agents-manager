/**
 * agent 停在編號選單上時，把選單畫成**可以直接點**的清單（2026-09-12 使用者回報）。
 *
 * 原本要在手機上選第 4 項，得在下面那排鍵按三次 ↓ 再按 Enter——而且選項到底寫什麼，得去戳
 * 終端快照裡 11px 的字，長一點的說明還被終端寬度截掉。這裡把 `parseChoiceMenu` 讀出來的
 * 問題、選項、被折掉的說明攤成一列一列 44px 的按鈕，點一下就答完。
 *
 * 認不出選單就什麼都不畫（回 `null`），畫面照舊退回終端快照＋按鍵面板——按鍵是直接送進別人
 * 終端的，寧可少一個捷徑，也不要在認錯的畫面上替使用者答題。
 */
import { useState } from 'react'
import { usePaneKeys } from '../hooks/usePaneKeys'
import { keysToSelect, parseChoiceMenu, sameChoices, type TuiChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'
import './blockedChoices.css'

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
  const [busy, setBusy] = useState<number | null>(null)
  const [stale, setStale] = useState(false)

  /**
   * 送出**前**先重讀一次畫面。
   *
   * ↓ 要按幾次是從「游標現在在哪」算出來的相對值，而手上這份快照最多是一秒前的。中間如果
   * 使用者自己在終端上動過、或 agent 換了一個問題，照舊資料按下去就會答到別的題目。所以
   * 按鍵永遠用當下重讀的那份算，而且比對過選項沒變才送；變了就只提示、什麼都不送。
   */
  const pick = async (target: number) => {
    if (busy !== null) return
    setBusy(target)
    setStale(false)
    try {
      const fresh = await readTerminal(botId, 'visible', 200)
      const now = parseChoiceMenu(fresh.text)
      if (!now || !sameChoices(now, menu)) {
        setStale(true)
        return
      }
      await sendKeys(botId, keysToSelect(now, target))
      onAnswered?.()
    } catch {
      setStale(true)
    } finally {
      setBusy(null)
    }
  }

  return (
    <div className="blocked-choices">
      {menu.question ? <p className="bc-question">{menu.question}</p> : null}
      <ol className="bc-list">
        {menu.choices.map((c, i) => (
          <li key={`${c.number}-${c.title}`}>
            <button
              type="button"
              className={`bc-item${c.current ? ' bc-current' : ''}`}
              disabled={busy !== null}
              aria-current={c.current ? 'true' : undefined}
              title={c.current ? '游標現在就停在這一項，點一下等於直接按 Enter' : '選這一項（送 ↓／↑ ＋ Enter）'}
              onClick={() => void pick(i)}
            >
              <span className="bc-num" aria-hidden="true">
                {c.number}
              </span>
              <span className="bc-title">{c.title}</span>
              <span className="bc-mark">{busy === i ? '送出中…' : c.current ? '游標在此' : ''}</span>
              {c.detail ? <span className="bc-detail">{c.detail}</span> : null}
            </button>
          </li>
        ))}
      </ol>
      {/* 平常不留任何說明文字（2026-09-12 第二輪回饋：畫面上只要「問題 ＋ 選項 ＋ 送出」）。
          「點一下會送什麼」寫在每顆按鈕的 tooltip；只有真的沒送出去時才需要一句話。 */}
      {stale ? (
        <p className="bc-hint bc-stale" role="status">
          畫面在這中間換過了，剛剛那一下沒有送出去——上面是重讀後的選單，請再點一次。
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
