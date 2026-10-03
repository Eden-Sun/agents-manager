import { useEffect, useRef, useState } from 'react'
import { botLamp, useStore } from '../store/store'
import { activityStartedAt, fmtElapsed } from '../lib/elapsed'
import { forgetRunElapsed, rememberRunElapsed, runElapsedAt } from '../lib/runElapsedCache'
import './runElapsed.css'


/** 跑多久了（2026-09-16 使用者：子 agent 執行中要有小字說已 run 幾分；同日改成只寫時間、放燈號下方）。只在 working 時出現。 */
/** 這一頁看到某顆 bot「開始在跑」的時刻；daemon 兩種紀錄都沒有時（升級前的舊列、或別人派的回合還沒
 * 建出 turn）才拿來墊底——issue #93：起點要以 daemon 觀察到的為準，這只是最後一道防線。 */
export function RunElapsed({ botId }: { botId: string }) {
  const startedAt = useStore((s) => {
    if (botLamp(s, botId) !== 'working') return null
    return activityStartedAt(s.runs[botId]?.agent_status_since, Object.values(s.turns[botId] ?? {}))
  })
  const working = useStore((s) => botLamp(s, botId) === 'working')
  // 每秒上數；沒在跑時不開計時器。
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
  // run 的 started_at 不能拿來當這回合的起點：bot 開著好幾小時不代表這回合跑了好幾小時。
  const from = startedAt ? new Date(startedAt).getTime() : (fallbackStartedAt.current ?? runElapsedAt(botId) ?? Date.now())
  const ms = Date.now() - from
  if (!Number.isFinite(ms) || ms < 0) return null
  return (
    <span
      className="run-elapsed"
      title={startedAt ? `從 ${new Date(startedAt).toLocaleTimeString()} 開始一直在跑（daemon 觀察到的時間）` : '從這個網頁看到它開始跑算起（daemon 還沒有這筆紀錄）'}
    >
      {fmtElapsed(ms)}
    </span>
  )
}
