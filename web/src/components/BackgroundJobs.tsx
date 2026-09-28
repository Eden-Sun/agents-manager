import { useStore } from '../store/store'
import { backgroundDetail, backgroundJobs, backgroundLabel, backgroundShortLabel } from '../lib/backgroundJobs'
import './backgroundJobs.css'

function useBackground(botId: string): { n: number; kind: string } {
  const n = useStore((s) => backgroundJobs(s.runs[botId]))
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  return { n, kind }
}

/**
 * #714：側欄與標題列上的「背景執行中（N）」。`variant="state"` 取代側欄那格「閒置」字；`mini` 給精簡的 child 列
 * （它們沒有狀態字，只有燈號）；`chip` 放標題列第二行。
 */
export function BackgroundJobsBadge({
  botId,
  variant,
  fallback = null,
}: {
  botId: string
  variant: 'state' | 'mini' | 'chip'
  /** 沒有背景工作時畫這個（側欄那格原本的「閒置」字）。 */
  fallback?: React.ReactNode
}) {
  const { n, kind } = useBackground(botId)
  if (n === 0) return <>{fallback}</>
  const title = backgroundDetail(kind, n)
  if (variant === 'mini') {
    return (
      <span className="bg-jobs-mini" title={title} aria-label={backgroundLabel(n)}>
        <span className="bg-jobs-dot" aria-hidden="true" />
        {n}
      </span>
    )
  }
  return (
    <span className={variant === 'state' ? 'bot-state bg-jobs-state' : 'bg-jobs-chip'} title={`${backgroundLabel(n)}：${title}`}>
      <span className="bg-jobs-dot" aria-hidden="true" />
      {variant === 'state' ? backgroundShortLabel(n) : backgroundLabel(n)}
    </span>
  )
}

/** 聊天最底下（輸入框上方）的一條說明：最後一則之後它其實還在動。 */
export function BackgroundJobsBar({ botId }: { botId: string }) {
  const { n, kind } = useBackground(botId)
  if (n === 0) return null
  return (
    <div className="bg-jobs-bar" role="status">
      <span className="bg-jobs-dot" aria-hidden="true" />
      <strong>{backgroundLabel(n)}</strong>
      <span>{backgroundDetail(kind, n)}</span>
    </div>
  )
}
