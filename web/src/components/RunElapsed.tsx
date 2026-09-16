import { useEffect, useState } from 'react'
import { botLamp, useStore } from '../store/store'
import './runElapsed.css'

/** 2m / 45m / 1h20 — 只到分，秒級跳動在側欄只會吵。 */
function fmtElapsed(ms: number): string {
  const m = Math.floor(ms / 60000)
  if (m < 1) return '<1m'
  if (m < 60) return `${m}m`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

/** 跑多久了（2026-09-16 使用者：子 agent 執行中要有小字說已 run 幾分；同日改成只寫時間、放燈號下方）。只在 working 時出現。 */
/** 這一頁看到某顆 bot「開始在跑」的時刻；daemon 沒有 turn 紀錄時（多半是別人派的回合）拿來墊底。 */
const seenWorkingAt = new Map<string, number>()

export function RunElapsed({ botId }: { botId: string }) {
  const startedAt = useStore((s) => {
    if (botLamp(s, botId) !== 'working') return null
    let start: string | null = null
    for (const t of Object.values(s.turns[botId] ?? {})) {
      if (t.status === 'in_flight' && (!start || t.created_at < start)) start = t.created_at
    }
    return start
  })
  const working = useStore((s) => botLamp(s, botId) === 'working')
  // 一分鐘一跳就夠；沒在跑時不開計時器。
  const [, tick] = useState(0)
  useEffect(() => {
    if (!working) {
      seenWorkingAt.delete(botId)
      return
    }
    if (!seenWorkingAt.has(botId)) seenWorkingAt.set(botId, Date.now())
    const id = setInterval(() => tick((n) => n + 1), 20000)
    return () => clearInterval(id)
  }, [working, botId])
  if (!working) return null
  // run 的 started_at 不能拿來當這回合的起點：bot 開著好幾小時不代表這回合跑了好幾小時。
  const from = startedAt ? new Date(startedAt).getTime() : (seenWorkingAt.get(botId) ?? Date.now())
  const ms = Date.now() - from
  if (!Number.isFinite(ms) || ms < 0) return null
  return (
    <span
      className="run-elapsed"
      title={startedAt ? `這一回合從 ${new Date(startedAt).toLocaleTimeString()} 開始跑` : '從這個網頁看到它開始跑算起（daemon 沒有這一回合的紀錄）'}
    >
      {fmtElapsed(ms)}
    </span>
  )
}
