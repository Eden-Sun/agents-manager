import type { Lamp } from '../api/types'
import { botStateLabel } from '../components/lampLabel'
import { backgroundJobs } from '../lib/backgroundJobs'
import { botLamp, useStore } from '../store/store'

/**
 * 一顆 bot 的燈號、背景工作數與狀態字（#714）。每個 selector 回純值（字串／數字）：回新物件會 getSnapshot should be cached。
 * 標題燈、BotSwitcher、群組成員、手機主力晶片都用這一支，狀態字跟側欄同一來源。
 */
export function useBotLamp(botId: string | null): { lamp: Lamp; background: number; label: string } {
  const lamp = useStore((s) => (botId ? botLamp(s, botId) : 'offline'))
  const background = useStore((s) => (botId && botLamp(s, botId) === 'idle' ? backgroundJobs(s.runs[botId]) : 0))
  const label = useStore((s) => (botId ? botStateLabel(botLamp(s, botId), s.runs[botId]) : botStateLabel('offline', null)))
  return { lamp, background, label }
}
