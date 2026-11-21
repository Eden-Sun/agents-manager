import type { Project } from '../api/types'
import { t } from './i18n'

export function handedOffTo(projects: readonly Project[], projectId: string | null | undefined): string | null {
  const to = projects.find((p) => p.id === projectId)?.handed_off_to
  return to && to.trim() ? to : null
}

export function handedOffReason(host: string): string {
  return t('由 {host} 管理：這裡不送訊息，也不啟動或停止這顆 Bot', { host })
}
