import type { Lamp, Run } from '../api/types'
import { backgroundJobs, backgroundLabel } from '../lib/backgroundJobs'

/** SPEC §2.2 colour rules. */
export const LAMP_LABEL: Record<Lamp, string> = {
  disconnected: '連線中斷',
  offline: '離線',
  starting: '啟動中',
  stopping: '停止中',
  idle: '閒置',
  working: '執行中',
  blocked: '等待回應',
  unknown: '狀態未知',
}

/**
 * 這顆 bot 現在的狀態字（#714）：燈號是 idle、而且回合結束後背景還有工作在跑時是「背景執行中（N）」，不然是 `LAMP_LABEL`。
 * 側欄、標題燈、BotSwitcher、群組成員、手機主力晶片都從這裡拿文案，不各寫一份「閒置」。
 * 只有 idle 才改：working／blocked／斷線時燈號已經說清楚了（斷線時畫面讀不到，背景數字也不可信）。
 */
export function botStateLabel(lamp: Lamp, run: Run | null | undefined): string {
  const n = lamp === 'idle' ? backgroundJobs(run) : 0
  return n > 0 ? backgroundLabel(n) : LAMP_LABEL[lamp]
}
