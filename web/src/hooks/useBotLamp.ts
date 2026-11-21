import type { Lamp } from '../api/types'
import { botStateLabel, isCompacting } from '../components/lampLabel'
import { backgroundJobs } from '../lib/backgroundJobs'
import { blockedReasonOf } from '../lib/blockedReason'
import { botLamp, useStore } from '../store/store'

export function useKidsRunning(botId: string | null): number {
  return useStore((s) =>
    botId ? s.bots.filter((b) => b.parent_bot_id === botId && ['working', 'blocked'].includes(botLamp(s, b.id))).length : 0,
  )
}

export function useBotLamp(botId: string | null): { lamp: Lamp; background: number; label: string; blockedReason: string; kids: number; compacting: boolean } {
  const lamp = useStore((s) => (botId ? botLamp(s, botId) : 'offline'))
  const background = useStore((s) => (botId && botLamp(s, botId) === 'idle' ? backgroundJobs(s.runs[botId]) : 0))
  const label = useStore((s) => (botId ? botStateLabel(botLamp(s, botId), s.runs[botId]) : botStateLabel('offline', null)))
  const blockedReason = useStore((s) => (botId && botLamp(s, botId) === 'blocked' ? blockedReasonOf(s.runs[botId]) : ''))
  const kids = useKidsRunning(botId)
  const compacting = useStore((s) => (botId ? isCompacting(botLamp(s, botId), s.runs[botId]) : false))
  return { lamp, background, label, blockedReason, kids, compacting }
}
