import { fmtTime } from '../lib/userTime'
import { useEffect, useRef, useState } from 'react'
import { useT } from '../lib/i18n'
import { botLamp, useStore } from '../store/store'
import { activityStartedAt, fmtElapsed } from '../lib/elapsed'
import { forgetRunElapsed, rememberRunElapsed, runElapsedAt } from '../lib/runElapsedCache'
import './runElapsed.css'

export function RunElapsed({ botId }: { botId: string }) {
  const t = useT()
  const startedAt = useStore((s) => {
    if (botLamp(s, botId) !== 'working') return null
    return activityStartedAt(s.runs[botId]?.agent_status_since, Object.values(s.turns[botId] ?? {}))
  })
  const working = useStore((s) => botLamp(s, botId) === 'working')
  const [, tick] = useState(0)
  const fallbackStartedAt = useRef<number | null>(null)
  useEffect(() => {
    if (!working) {
      forgetRunElapsed(botId)
      fallbackStartedAt.current = null
      return
    }
    const remembered = runElapsedAt(botId)
    const fallback = remembered ?? Date.now()
    fallbackStartedAt.current ??= fallback
    rememberRunElapsed(botId, fallback)
    const id = setInterval(() => tick((n) => n + 1), 1000)
    return () => {
      clearInterval(id)
      if (!useStore.getState().bots.some((bot) => bot.id === botId)) forgetRunElapsed(botId)
    }
  }, [working, botId])
  if (!working) return null
  const from = startedAt ? new Date(startedAt).getTime() : (fallbackStartedAt.current ?? runElapsedAt(botId) ?? Date.now())
  const ms = Date.now() - from
  if (!Number.isFinite(ms) || ms < 0) return null
  return (
    <span
      className="run-elapsed"
      title={startedAt ? t('從 {time} 開始一直在跑（daemon 觀察到的時間）', { time: fmtTime(new Date(startedAt)) }) : t('從這個網頁看到它開始跑算起（daemon 還沒有這筆紀錄）')}
    >
      {fmtElapsed(ms)}
    </span>
  )
}
