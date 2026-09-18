import { useRef } from 'react'
import type { MouseEvent, PointerEvent } from 'react'
import { copyText } from '../lib/copyText'

const INTERACTIVE = 'a, button, input, textarea, select, [role="button"], .msg-attachments, .msg-snapshot'
const hasSelection = () => Boolean(window.getSelection()?.toString())

/** 短按複製；長按、拖選、捲動與訊息內的連結／按鈕保留瀏覽器原生行為。 */
export function useTapCopy(text: string, onCopied: (ok: boolean) => void) {
  const press = useRef<{ id: number; x: number; y: number; at: number; cancelled: boolean } | null>(null)
  const cancel = () => { press.current = null }
  const move = (e: PointerEvent<HTMLElement>) => {
    const p = press.current
    if (p && (e.pointerId !== p.id || Math.hypot(e.clientX - p.x, e.clientY - p.y) > 8)) p.cancelled = true
  }

  return {
    onPointerDown: (e: PointerEvent<HTMLElement>) => {
      if (!e.isPrimary || e.button !== 0 || hasSelection() || (e.target as Element).closest(INTERACTIVE)) {
        cancel()
        return
      }
      press.current = { id: e.pointerId, x: e.clientX, y: e.clientY, at: performance.now(), cancelled: false }
    },
    onPointerMove: move,
    onPointerUp: move,
    onPointerCancel: cancel,
    onPointerLeave: cancel,
    onContextMenu: cancel,
    onClick: (e: MouseEvent<HTMLElement>) => {
      const p = press.current
      cancel()
      if (!p || p.cancelled || performance.now() - p.at >= 450 || hasSelection() || !text.trim()) return
      if ((e.target as Element).closest(INTERACTIVE)) return
      void copyText(text).then(onCopied)
    },
  }
}
