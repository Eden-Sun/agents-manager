import { useState } from 'react'
import { buildCounts, buildTooltip } from '../lib/buildSlotsText'
import { useT } from '../lib/i18n'
import { useBuildSlots } from '../store/buildSlots'
import { useStore } from '../store/store'
import './buildSlotsChip.css'

export function BuildSlotsChip() {
  const t = useT()
  const snap = useBuildSlots()
  const bots = useStore((s) => s.bots)
  const [now, setNow] = useState(() => Date.now())
  if (!snap) return null
  const { active, waiting } = buildCounts(snap)
  const botName = (id: string | null) => (id ? (bots.find((b) => b.id === id)?.name ?? id.slice(-6)) : t('人工'))
  const cls = `mem-badge build-slots-chip${active === 0 && waiting === 0 ? ' idle' : ''}${waiting > 0 ? ' queued' : ''}`
  return (
    <span
      className={cls}
      data-k="build-slots-chip"
      role="status"
      aria-label={t('建置 {a}＋{w} 排隊', { a: active, w: waiting })}
      title={buildTooltip(snap, botName, now, t)}
      onMouseEnter={() => setNow(Date.now())}
      onFocus={() => setNow(Date.now())}
      tabIndex={0}
    >
      <span className="mem-k">{t('建置')}</span>
      <span className="mem-v">
        {active}
        {waiting > 0 ? <span className="build-slots-wait">＋{waiting}</span> : null}
      </span>
    </span>
  )
}
