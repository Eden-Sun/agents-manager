import type { Lamp } from '../api/types'
import { botStateLabel } from '../components/lampLabel'
import { backgroundJobs } from '../lib/backgroundJobs'
import { blockedReasonOf } from '../lib/blockedReason'
import { botLamp, useStore } from '../store/store'

/**
 * 一顆 bot 的燈號、背景工作數與狀態字（#714）。每個 selector 回純值（字串／數字）：回新物件會 getSnapshot should be cached。
 * 標題燈、BotSwitcher、群組成員、手機主力晶片都用這一支，狀態字跟側欄同一來源。
 */
/** 底下在跑（working／blocked）的子 agent 數：燈號外圈只為這個轉（2026-10-04 使用者：「沒有 child 也在轉，轉個毛」）。 */
export function useKidsRunning(botId: string | null): number {
  return useStore((s) =>
    botId ? s.bots.filter((b) => b.parent_bot_id === botId && ['working', 'blocked'].includes(botLamp(s, b.id))).length : 0,
  )
}

export function useBotLamp(botId: string | null): { lamp: Lamp; background: number; label: string; blockedReason: string; kids: number } {
  const lamp = useStore((s) => (botId ? botLamp(s, botId) : 'offline'))
  const background = useStore((s) => (botId && botLamp(s, botId) === 'idle' ? backgroundJobs(s.runs[botId]) : 0))
  const label = useStore((s) => (botId ? botStateLabel(botLamp(s, botId), s.runs[botId]) : botStateLabel('offline', null)))
  // blocked 的原因（daemon 的 `run.blocked_reason.text`）；不是 blocked 或沒有原因是空字串。
  const blockedReason = useStore((s) => (botId && botLamp(s, botId) === 'blocked' ? blockedReasonOf(s.runs[botId]) : ''))
  const kids = useKidsRunning(botId)
  return { lamp, background, label, blockedReason, kids }
}
