import { fmtDateTime } from '../lib/userTime'
import { useT } from '../lib/i18n'
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useShallow } from 'zustand/react/shallow'
import { projectHostName, useStore } from '../store/store'
import { canonicalModel, quotaKey } from '../api/types'
import { resendTarget } from '../lib/resendTarget'
import { modelAtCapacity } from '../lib/modelCapacity'
import './turnErrorBadge.css'

const opusId = () => canonicalModel('claude', 'opus')

export function TurnErrorBadge({ botId }: { botId: string }) {
  const t = useT()
  const notice = useStore((s) => s.runs[botId]?.turn_error ?? null)
  const capacity = useStore((s) => modelAtCapacity(s.runs[botId]))
  const quotaLimit = notice !== null && /^you've reached your .*limit/i.test(notice)
  const quotaName = notice && /fable/i.test(notice) ? 'Fable' : t('5 小時')
  const resetsAt = useStore((s) => {
    if (!quotaLimit) return null
    const bot = s.bots.find((b) => b.id === botId)
    if (!bot) return null
    const base = bot.identity ? `${bot.kind}:${bot.identity}` : bot.kind
    const q = s.quota[quotaKey(projectHostName(s, bot.project_id), base)]
    if (!q) return null
    return q.limit_hit?.until ?? (/fable/i.test(notice ?? '') ? q.fable?.resets_at : q.five_hour?.resets_at) ?? null
  })
  const currentModel = useStore((s) => s.bots.find((b) => b.id === botId)?.model ?? null)
  const isOpus = currentModel === opusId() || currentModel === 'opus'
  const patchBot = useStore((s) => s.patchBot)
  const resendFields = useStore(useShallow((s) => {
    const target = resendTarget(s.messages[botId] ?? [])
    return { text: target?.text ?? null, attachmentsJson: target ? JSON.stringify(target.attachments) : null }
  }))
  const target = useMemo(() => resendFields.text === null ? null : {
    text: resendFields.text,
    attachments: resendFields.attachmentsJson ? JSON.parse(resendFields.attachmentsJson) as string[] : [],
  }, [resendFields])
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

  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => popRef.current?.focus({ preventScroll: true }))
    return () => cancelAnimationFrame(raf)
  }, [open])
  const closePop = () => {
    if (popRef.current?.contains(document.activeElement)) btnRef.current?.focus()
    setOpen(false)
  }
  useEffect(() => {
    if (!open) return
    const away = (e: MouseEvent) => {
      const t = e.target as Node
      if (btnRef.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const esc = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closePop()
    }
    document.addEventListener('mousedown', away)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', away)
      document.removeEventListener('keydown', esc)
    }
  }, [open])

  if (!notice || capacity) return null

  const resend = () => {
    if (!target) return
    setSending(true)
    void sendPrompt(botId, target.text, target.attachments).then((ok) => {
      setSending(false)
      if (ok) {
        closePop()
        notify('info', t('已重送上一則，接著跑同一回合的內容'))
      }
    })
  }

  const pop = open ? (
    <div
      ref={popRef}
      className="turn-error-pop"
      role="dialog"
      tabIndex={-1}
      aria-label={t("回合被中斷")}
      style={pos ? { left: pos.left, top: pos.top } : { left: 0, top: 0, visibility: 'hidden' }}
    >
      {quotaLimit ? (
        <p className="turn-error-why">
          {resetsAt
            ? t('這一回合根本沒跑：{quotaName} 額度已用盡，claude 直接拒絕。重送沒有用——等 {time} 重置，或先換一個模型繼續。', {
                quotaName,
                time: fmtDateTime(new Date(resetsAt), { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' }),
              })
            : t('這一回合根本沒跑：{quotaName} 額度已用盡，claude 直接拒絕。重送沒有用——等額度重置，或先換一個模型繼續。', { quotaName })}
        </p>
      ) : (
        <p className="turn-error-why">
          {t.x('這一回合被 API 連線中斷截斷，回應是不完整的——終端與 hook 都會把它報成 {done}，所以側欄的燈號看起來一切正常。', {
            done: <code>done</code>,
          })}
        </p>
      )}
      {}
      <pre className="turn-error-raw">{notice}</pre>
      <div className="turn-error-act">
        {quotaLimit ? (
          <button
            type="button"
            className="btn primary"
            disabled={sending || isOpus}
            title={isOpus ? t('已經是 opus') : t('把這顆 bot 的模型改成 opus（live 套用，不重啟）')}
            onClick={() => {
              setSending(true)
              void patchBot(botId, { model: opusId() }).then((res) => {
                setSending(false)
                if (res) {
                  closePop()
                  notify('info', t('已改用 opus，重送上一則就能繼續'))
                }
              })
            }}
          >
            {sending ? t('切換中…') : t('改用 opus')}
          </button>
        ) : null}
        <button
          type="button"
          className="btn primary"
          disabled={!target || sending || busy || (quotaLimit && !isOpus && !resetPassed(resetsAt))}
          title={
            !target
              ? t('這個對話裡沒有可以重送的訊息')
              : quotaLimit && !isOpus && !resetPassed(resetsAt)
                ? t('額度還沒重置，重送只會再被拒絕一次；先換模型')
              : busy
                ? t('它正在忙，等這一輪停下來再重送')
                : t('重送：{preview}', { preview: Array.from(target.text).slice(0, 60).join('') || t('（{n} 個附件）', { n: target.attachments.length }) })
          }
          onClick={resend}
        >
          {sending ? t('重送中…') : t('重送上一則')}
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
        {quotaLimit ? t('⛔ {quotaName} 額度用盡', { quotaName }) : t('⚠ 回合被中斷（API 錯誤）')}
      </button>
      {pop && createPortal(pop, document.body)}
    </>
  )
}

function resetPassed(resetsAt: string | null): boolean {
  if (!resetsAt) return false
  const t = Date.parse(resetsAt)
  return Number.isFinite(t) && t <= Date.now()
}
