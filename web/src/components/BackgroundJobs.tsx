import { useStore } from '../store/store'
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
  // #774：跑了多久（每次 store 更新時重算；開始時間本身不會過期）、daemon 判定的「可能卡住」。
  const age = useStore((s) => backgroundAge(s.runs[botId]))
  const stuck = useStore((s) => backgroundStuck(s.runs[botId]))
  return { n, kind, age, stuck }
}

function ageText(age: string | null): string {
  return age ? `已持續 ${age}。` : ''
}

/**
 * #714：側欄上的「背景 N」。`variant="state"` 取代側欄那格「閒置」字；`mini` 給精簡的 child 列
 * （它們沒有狀態字，只有燈號）。標題列不放（2026-09-30 使用者：側欄與輸入框上方那條已經講了）。
 */
export function BackgroundJobsBadge({
  botId,
  variant,
  fallback = null,
}: {
  botId: string
  variant: 'state' | 'mini'
  /** 沒有背景工作時畫這個（側欄那格原本的「閒置」字）。 */
  fallback?: React.ReactNode
}) {
  const { n, kind, age, stuck } = useBackground(botId)
  if (n === 0) return <>{fallback}</>
  const title = backgroundDetail(kind, n) + ageText(age)
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
    <span className={`bot-state bg-jobs-state${stuck ? ' bg-jobs-stuck' : ''}`} title={`${label}：${title}`}>
      <span className="bg-jobs-dot" aria-hidden="true" />
      {backgroundShortLabel(n)}
    </span>
  )
}

/** 聊天最底下（輸入框上方）的一條說明：最後一則之後它其實還在動。 */
export function BackgroundJobsBar({ botId }: { botId: string }) {
  const { n, kind, age, stuck } = useBackground(botId)
  const lines = useStore((s) => backgroundTaskLines(s.runs[botId]).join('\n'))
  const cron = useStore((s) => cronLabel(s.runs[botId]))
  if (n === 0) return null
  return (
    <div className={`bg-jobs-bar${stuck ? ' bg-jobs-stuck' : ''}`} role="status">
      <span className="bg-jobs-dot" aria-hidden="true" />
      <strong>{stuck ? backgroundStuckLabel(n) : backgroundLabel(n)}</strong>
      <span>
        {stuck ? `已持續 ${age ?? '超過 3 小時'}，可能卡住或忘了收；要停就到 pane 裡處理。` : backgroundDetail(kind, n) + ageText(age)}
      </span>
      {lines && <span className="bg-jobs-tasks">{lines.split('\n').join(' · ')}</span>}
      {cron && <span className="bg-jobs-tasks">{cron}</span>}
    </div>
  )
}
