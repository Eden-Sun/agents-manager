import { useEffect, useState } from 'react'
import { t } from '../lib/i18n'

export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  if (h > 0) return t('{h} 小時 {m} 分', { h, m })
  if (m > 0) return t('{m} 分 {s} 秒', { m, s })
  return t('{s} 秒', { s })
}

interface HasDuration { duration_ms?: number | null; running_since?: string | null }

export function totalDurationMs(o: HasDuration, nowMs: number): number | null {
  if (o.duration_ms == null) return null
  const since = o.running_since ? Date.parse(o.running_since) : NaN
  return o.duration_ms + (Number.isNaN(since) ? 0 : Math.max(0, nowMs - since))
}

export function useNowMs(active: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    setNow(Date.now())
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [active])
  return now
}
