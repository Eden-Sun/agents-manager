import { useState } from 'react'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import {
  backgroundAge,
  backgroundDetail,
  backgroundJobs,
  backgroundLabel,
  backgroundShortLabel,
  backgroundStuck,
  backgroundStuckLabel,
  backgroundTaskLines,
  cronLabel,
} from '../lib/backgroundJobs'
import './backgroundJobs.css'

function useBackground(botId: string): { n: number; kind: string; age: string | null; stuck: boolean } {
  const n = useStore((s) => backgroundJobs(s.runs[botId]))
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  const age = useStore((s) => backgroundAge(s.runs[botId]))
  const stuck = useStore((s) => backgroundStuck(s.runs[botId]))
  return { n, kind, age, stuck }
}

export function BackgroundJobsBadge({
  botId,
  variant,
  fallback = null,
}: {
  botId: string
  variant: 'state' | 'mini'
  fallback?: React.ReactNode
}) {
  const t = useT()
  const { n, kind, age, stuck } = useBackground(botId)
  if (n === 0) return <>{fallback}</>
  const ageT = age ? t('已持續 {age}。', { age }) : ''
  const title = backgroundDetail(kind, n) + ageT
  const label = stuck ? backgroundStuckLabel(n) : backgroundLabel(n)
  if (variant === 'mini') {
    return (
      <span className={`bg-jobs-mini${stuck ? ' bg-jobs-stuck' : ''}`} title={title} aria-label={label}>
        <span className="bg-jobs-dot" aria-hidden="true" />
        {n}
      </span>
    )
  }
  return (
    <span className={`bot-state bg-jobs-state${stuck ? ' bg-jobs-stuck' : ''}`} title={t('{label}：{title}', { label, title })}>
      <span className="bg-jobs-dot" aria-hidden="true" />
      {backgroundShortLabel(n)}
    </span>
  )
}

export function BackgroundJobsBar({ botId }: { botId: string }) {
  const t = useT()
  const { n, kind, age, stuck } = useBackground(botId)
  const lines = useStore((s) => backgroundTaskLines(s.runs[botId]).join('\n'))
  const cron = useStore((s) => cronLabel(s.runs[botId]))
  const [open, setOpen] = useState(false)
  if (n === 0) return null
  const ageT = age ? t('已持續 {age}。', { age }) : ''
  const detail = stuck ? t('已持續 {age}，可能卡住或忘了收；要停就到 pane 裡處理。', { age: age ?? t('超過 1 小時') }) : backgroundDetail(kind, n) + ageT
  return (
    <div className={`bg-jobs-bar${stuck ? ' bg-jobs-stuck' : ''}${open ? ' open' : ''}`} role="status">
      <button type="button" className="bg-jobs-summary" aria-expanded={open} title={detail} onClick={() => setOpen((v) => !v)}>
        <span className="bg-jobs-dot" aria-hidden="true" />
        <strong>{stuck ? backgroundStuckLabel(n) : backgroundLabel(n)}</strong>
        {age && <span className="bg-jobs-age">{age}</span>}
        {lines && <span className="bg-jobs-tasks">{lines.split('\n').join(' · ')}</span>}
      </button>
      <span className="bg-jobs-detail">
        {detail}
        {cron ? ` ${cron}` : ''}
      </span>
    </div>
  )
}
