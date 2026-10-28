import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { createPortal } from "react-dom"
import * as api from "../api"
import { ApiError } from "../api/types"
import { useStore } from "../store/store"
import { compactRefusal } from "../lib/compactRefusal"
import { useT } from "../lib/i18n"
import "./compactButton.css"

/**
 * context 旁的「壓縮」鈕（2026-10-04 使用者：「可以對某個 bot 下 compact，按鈕做在 context 旁邊」）：對閒著的 bot 送 `/compact`。
 * 只在 claude／codex、bot 在跑而且閒著時能按；壓完的新用量由 statusLine 回報，這裡不等。
 */
export function CompactButton({ botId }: { botId: string }) {
  const t = useT()
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const idle = useStore((s) => {
    const r = s.runs[botId]
    return Boolean(r && r.state === "running" && r.agent_status === "idle")
  })
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  if (kind !== "claude" && kind !== "codex") return null
  const go = async () => {
    setBusy(true)
    try {
      await api.compactBot(botId)
      notify("info", t("已送出 /compact，壓縮完 context 用量會更新"))
    } catch (e) {
      const reason = e instanceof ApiError ? String(e.body.reason ?? e.message) : String(e)
      notify("error", t("沒有送出：{reason}", { reason: compactRefusal(reason) }))
    } finally {
      setBusy(false)
    }
  }
  return (
    <button
      type="button"
      className="compact-btn"
      disabled={!idle || busy}
      title={idle ? t("對這顆 bot 送 /compact，壓縮對話脈絡") : t("要等它閒下來才能壓縮")}
      onClick={(e) => {
        e.stopPropagation()
        void go()
      }}
    >
      {busy ? t("送出中…") : t("壓縮")}
    </button>
  )
}

/** A single context entry point, including when a Codex reading is unavailable. */
export function ContextCard({ botId, children }: { botId: string; children: ReactNode }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const idle = useStore((s) => s.runs[botId]?.state === 'running' && s.runs[botId]?.agent_status === 'idle')
  const [pos, setPos] = useState({ top: 0, left: 0 })
  const trigger = useRef<HTMLButtonElement>(null)
  const card = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    if (!open) return
    const place = () => {
      const r = trigger.current?.getBoundingClientRect()
      if (!r) return
      const width = Math.min(280, window.innerWidth - 16)
      setPos({ left: Math.max(8, Math.min(r.left, window.innerWidth - width - 8)), top: Math.min(r.bottom + 6, window.innerHeight - 130) })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => { window.removeEventListener('resize', place); window.removeEventListener('scroll', place, true) }
  }, [open])
  useEffect(() => {
    if (!open) return
    const outside = (e: MouseEvent) => { if (!trigger.current?.contains(e.target as Node) && !card.current?.contains(e.target as Node)) setOpen(false) }
    const key = (e: KeyboardEvent) => { if (e.key === 'Escape') { e.preventDefault(); setOpen(false); trigger.current?.focus() } }
    document.addEventListener('mousedown', outside)
    document.addEventListener('keydown', key)
    return () => { document.removeEventListener('mousedown', outside); document.removeEventListener('keydown', key) }
  }, [open])
  return <>
    <button ref={trigger} type="button" className="context-card-trigger" aria-haspopup="dialog" aria-expanded={open} onClick={() => setOpen(!open)}>{children} ▾</button>
    {open ? createPortal(<div ref={card} className="context-card" onMouseDown={(e) => e.stopPropagation()} role="dialog" aria-label="context" style={{ top: pos.top, left: pos.left }}>
      <strong>context</strong><div>{children}</div><CompactButton botId={botId} />
      {!idle ? <p>{t('要等它閒下來才能壓縮')}</p> : null}
    </div>, document.body) : null}
  </>
}
