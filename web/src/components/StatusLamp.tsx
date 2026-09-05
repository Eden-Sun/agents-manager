import type { Lamp } from '../api/types'

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

export function StatusLamp({ lamp, title }: { lamp: Lamp; title?: string }) {
  return (
    <span
      className={`lamp lamp-${lamp}`}
      role="img"
      aria-label={title ?? LAMP_LABEL[lamp]}
      title={title ?? LAMP_LABEL[lamp]}
    />
  )
}
