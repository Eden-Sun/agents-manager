import { useShallow } from 'zustand/react/shallow'
import type { BotKind, KindQuota, QuotaWindow } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'

/**
 * v4.0 remaining quota per kind (`GET /api/quota` + WS `quota_updated`), centred in the
 * main header: `5h 剩 82% · 7d 剩 60%`. Below 20% remaining turns to the warning colour.
 * Identity-scoped entries (`claude:cc1`) hang under their kind in small type.
 */

const LOW = 20

function remaining(w: QuotaWindow | null): number | null {
  return w ? Math.max(0, Math.round(100 - w.used_pct)) : null
}

function fmtTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso || '—'
  return d.toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

function tooltip(label: string, q: KindQuota | null): string {
  if (!q) return `${label}：沒有額度資訊`
  const lines = [label]
  if (q.five_hour) lines.push(`5 小時：已用 ${q.five_hour.used_pct}%，${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q.seven_day) lines.push(`7 天：已用 ${q.seven_day.used_pct}%，${fmtTime(q.seven_day.resets_at)} 重置`)
  if (q.plan) lines.push(`方案：${q.plan}`)
  if (q.updated_at) lines.push(`更新：${fmtTime(q.updated_at)}`)
  return lines.join('\n')
}

function Windows({ q }: { q: KindQuota | null }) {
  if (!q || (!q.five_hour && !q.seven_day)) return <span className="quota-none">—</span>
  const r5 = remaining(q.five_hour)
  const r7 = remaining(q.seven_day)
  return (
    <>
      {r5 !== null ? (
        <span className={`quota-win${r5 < LOW ? ' low' : ''}`}>
          <span className="quota-k">5h</span> 剩 {r5}%
        </span>
      ) : null}
      {r5 !== null && r7 !== null ? <span className="quota-sep">·</span> : null}
      {r7 !== null ? (
        <span className={`quota-win${r7 < LOW ? ' low' : ''}`}>
          <span className="quota-k">7d</span> 剩 {r7}%
        </span>
      ) : null}
    </>
  )
}

function QuotaBadge({ kind }: { kind: BotKind }) {
  const q = useStore((s) => s.quota[kind] ?? null)
  const known = useStore((s) => kind in s.quota)
  // `claude:cc1` style entries for this kind.
  const scoped = useStore(
    useShallow((s) =>
      Object.keys(s.quota)
        .filter((k) => k.startsWith(`${kind}:`))
        .sort(),
    ),
  )
  const scopedMap = useStore(useShallow((s) => Object.fromEntries(scoped.map((k) => [k, s.quota[k] ?? null]))))
  if (!known && scoped.length === 0) return null
  const low = [q?.five_hour, q?.seven_day].some((w) => w && 100 - w.used_pct < LOW)
  return (
    <span className={`quota-badge${low ? ' low' : ''}${!q ? ' empty' : ''}`} title={tooltip(kind, q)}>
      <span className="quota-main">
        <KindTag kind={kind} />
        <Windows q={q} />
      </span>
      {scoped.map((k) => (
        <span className="quota-sub" key={k} title={tooltip(k, scopedMap[k])}>
          <span className="quota-id">{k.slice(kind.length + 1)}</span>
          <Windows q={scopedMap[k]} />
        </span>
      ))}
    </span>
  )
}

export function QuotaStrip() {
  const any = useStore((s) => Object.keys(s.quota).length > 0)
  if (!any) return null
  return (
    <div className="quota-strip" aria-label="各 kind 剩餘額度">
      {BOT_KINDS.map((k) => (
        <QuotaBadge key={k} kind={k} />
      ))}
    </div>
  )
}
