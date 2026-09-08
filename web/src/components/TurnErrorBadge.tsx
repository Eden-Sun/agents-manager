import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useStore } from '../store/store'

/**
 * 「這一回合其實斷了」的紅色 badge（SPEC §4.3a）。
 *
 * claude 的連線在回應中途掉了只會在 pane 印一行
 * `⏺ API Error: Connection lost mid-response. The response above may be incomplete.`，
 * 然後照常收工——hook 送 Stop、herdr 報 idle，側欄一顆綠燈，使用者以為做完了。daemon 把那行
 * 讀出來掛在 run 上（`runs.turn_error`），這顆 badge 就是它在畫面上的樣子。
 *
 * 不做成 tooltip：斷掉的回合是要**動手**的，不是滑過去才知道的。所以是一顆點得下去的紅 chip，
 * 點開看得到原文，底下就是「重送上一則」——走的是輸入框那條 `sendPrompt`，沒有另一套送法。
 *
 * 沒有「知道了」：下一回合一開就會被 daemon 清掉（`arm_progress`），重送本身就是清掉它的動作。
 *
 * 彈出層跟 `ModelQuickPicker` 一樣 portal 到 body 再用 `position: fixed` 定位：標題列那一列
 * 自己會疊在對話區底下，`position: absolute` 的浮層會被壓在後面（實測看得到一角）。
 */
export function TurnErrorBadge({ botId }: { botId: string }) {
  const notice = useStore((s) => s.runs[botId]?.turn_error ?? null)
  // 上一則使用者訊息就是「被斷掉的那一則」——重送指的是它。
  const lastUserText = useStore((s) => {
    const list = s.messages[botId] ?? []
    for (let i = list.length - 1; i >= 0; i -= 1) {
      if (list[i].role === 'user') return list[i].content
    }
    return null
  })
  const busy = useStore((s) => {
    const st = s.runs[botId]?.agent_status
    return st === 'working' || st === 'blocked'
  })
  const sendPrompt = useStore((s) => s.sendPrompt)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [sending, setSending] = useState(false)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)

  useLayoutEffect(() => {
    if (!open) {
      setPos(null)
      return
    }
    const place = () => {
      const el = btnRef.current
      if (!el) return
      const r = el.getBoundingClientRect()
      const margin = 8
      const w = popRef.current?.offsetWidth ?? 380
      const h = popRef.current?.offsetHeight ?? 200
      let left = Math.max(margin, Math.min(r.left, window.innerWidth - w - margin))
      if (left < margin) left = margin
      let top = r.bottom + 6
      if (top + h + margin > window.innerHeight) top = Math.max(margin, r.top - 6 - h)
      setPos({ left, top })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open])

  // 點外面 / Esc 收起來，跟其他浮層一樣。
  useEffect(() => {
    if (!open) return
    const away = (e: MouseEvent) => {
      const t = e.target as Node
      if (btnRef.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const esc = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', away)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', away)
      document.removeEventListener('keydown', esc)
    }
  }, [open])

  if (!notice) return null

  const resend = () => {
    if (!lastUserText) return
    setSending(true)
    void sendPrompt(botId, lastUserText).then((ok) => {
      setSending(false)
      if (ok) {
        setOpen(false)
        notify('info', '已重送上一則，接著跑同一回合的內容')
      }
    })
  }

  const pop = open ? (
    <div
      ref={popRef}
      className="turn-error-pop"
      role="dialog"
      aria-label="回合被中斷"
      style={pos ? { left: pos.left, top: pos.top } : { left: 0, top: 0, visibility: 'hidden' }}
    >
      <p className="turn-error-why">
        這一回合被 API 連線中斷截斷，回應是不完整的——終端與 hook 都會把它報成 <code>done</code>
        ，所以側欄的燈號看起來一切正常。
      </p>
      {/* 原文照貼：使用者要能拿它去對終端上那一行。 */}
      <pre className="turn-error-raw">{notice}</pre>
      <div className="turn-error-act">
        <button
          type="button"
          className="btn primary"
          disabled={!lastUserText || sending || busy}
          title={
            !lastUserText
              ? '這個對話裡沒有可以重送的訊息'
              : busy
                ? '它正在忙，等這一輪停下來再重送'
                : `重送：${lastUserText.slice(0, 60)}`
          }
          onClick={resend}
        >
          {sending ? '重送中…' : '重送上一則'}
        </button>
      </div>
    </div>
  ) : null

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className="turn-error-badge"
        aria-haspopup="dialog"
        aria-expanded={open}
        title={notice}
        onClick={() => setOpen((v) => !v)}
      >
        ⚠ 回合被中斷（API 錯誤）
      </button>
      {pop && createPortal(pop, document.body)}
    </>
  )
}
