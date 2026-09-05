import { useEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, KindQuota, QuotaWindow } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 remaining quota per kind (`GET /api/quota` + WS `quota_updated`).
 * Each kind is an independent ~132×40 pill; below 1600px only the focused /
 * lowest-remaining kind stays visible, with the rest in「全部額度」.
 */

function remaining(w: QuotaWindow | null): number | null {
  return w ? Math.max(0, Math.round(100 - w.used_pct)) : null
}

function fmtTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso || '—'
  return d.toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

function windowLine(name: string, w: QuotaWindow | null): string | null {
  if (!w) return null
  const reset = w.resets_at ? `${fmtTime(w.resets_at)} 重置` : '重置時間未知'
  return `${name}：已用 ${w.used_pct}%，${reset}`
}

function tooltip(label: string, q: KindQuota | null): string {
  if (!q) return `${label}：沒有額度資訊`
  const lines = [label]
  const five = windowLine('5 小時', q.five_hour)
  const seven = windowLine('7 天', q.seven_day)
  if (five) lines.push(five)
  if (seven) lines.push(seven)
  if (q.plan) lines.push(`方案：${q.plan}`)
  if (q.updated_at) lines.push(`更新：${fmtTime(q.updated_at)}`)
  return lines.join('\n')
}

function lowestRemaining(q: KindQuota | null): number {
  const vals = [remaining(q?.five_hour ?? null), remaining(q?.seven_day ?? null)].filter((n): n is number => n !== null)
  return vals.length ? Math.min(...vals) : 100
}

function levelClass(pct: number | null): string {
  if (pct === null) return ''
  if (pct < 10) return 'crit'
  if (pct < 30) return 'warn'
  return 'ok'
}

function QuotaPill({ kind }: { kind: BotKind }) {
  const q = useStore((s) => s.quota[kind] ?? null)
  const known = useStore((s) => kind in s.quota)
  const r5 = remaining(q?.five_hour ?? null)
  const r7 = remaining(q?.seven_day ?? null)
  const low = lowestRemaining(q)
  const pillLevel = !known || (!q?.five_hour && !q?.seven_day) ? '' : levelClass(low)

  return (
    <span className={`quota-pill${pillLevel ? ` ${pillLevel}` : ''}`} title={tooltip(kind, q)}>
      <span className="quota-pill-left">
        <KindTag kind={kind} />
        <span className="quota-kind-name">{kind}</span>
      </span>
      <span className="quota-pill-right">
        {!known || (!q?.five_hour && !q?.seven_day) ? (
          <span className="quota-none">無資料</span>
        ) : (
          <>
            {r5 !== null ? <span className={`quota-pct ${levelClass(r5)}`}>5h {r5}%</span> : <span className="quota-none">5h 無資料</span>}
            {r7 !== null ? <span className={`quota-pct ${levelClass(r7)}`}>7d {r7}%</span> : <span className="quota-none">7d 無資料</span>}
          </>
        )}
      </span>
    </span>
  )
}

export function QuotaStrip({ focusKind }: { focusKind?: BotKind | null }) {
  const quota = useStore((s) => s.quota)
  const any = Object.keys(quota).length > 0
  const [narrow, setNarrow] = useState(() => (typeof window !== 'undefined' ? window.innerWidth < 1600 : false))
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const onResize = () => setNarrow(window.innerWidth < 1600)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  const primaryKind = useMemo(() => {
    if (focusKind && focusKind in quota) return focusKind
    let best: BotKind | null = null
    let bestVal = Infinity
    for (const k of BOT_KINDS) {
      if (!(k in quota)) continue
      const v = lowestRemaining(quota[k] ?? null)
      if (v < bestVal) {
        bestVal = v
        best = k
      }
    }
    return best ?? BOT_KINDS.find((k) => k in quota) ?? null
  }, [focusKind, quota])

  if (!any || !primaryKind) return null

  const hidden = narrow ? BOT_KINDS.filter((k) => k in quota && k !== primaryKind) : []

  return (
    <div className="quota-strip" aria-label="各 kind 剩餘額度" ref={wrap}>
      {narrow ? (
        <>
          <QuotaPill kind={primaryKind} />
          {hidden.length > 0 ? (
            <div className="quota-more">
              <button type="button" className="quota-more-btn" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
                全部額度
              </button>
              {open ? (
                <div className="quota-pop" role="dialog" aria-label="全部額度">
                  {BOT_KINDS.filter((k) => k in quota).map((k) => (
                    <QuotaPill key={k} kind={k} />
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}
        </>
      ) : (
        BOT_KINDS.filter((k) => k in quota).map((k) => <QuotaPill key={k} kind={k} />)
      )}
    </div>
  )
}
