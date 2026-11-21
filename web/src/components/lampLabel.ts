import type { Lamp, Run } from '../api/types'
import { backgroundJobs, backgroundLabel } from '../lib/backgroundJobs'
import { blockedReasonOf } from '../lib/blockedReason'
import { t, tr } from '../lib/i18n'

const msg = <K extends string>(key: K): K => key

export const LAMP_LABEL: Record<Lamp, string> = {
  disconnected: msg('連線中斷'),
  offline: msg('離線'),
  starting: msg('啟動中'),
  stopping: msg('停止中'),
  idle: msg('閒置'),
  working: msg('執行中'),
  blocked: msg('等待回應'),
  unknown: msg('狀態未知'),
}

export const COMPACTING_LABEL = msg('壓縮中…')

export function isCompacting(lamp: Lamp, run: Run | null | undefined): boolean {
  return (lamp === 'working' || lamp === 'idle') && Boolean(run?.compacting_since)
}

export function botStateLabel(lamp: Lamp, run: Run | null | undefined): string {
  if (isCompacting(lamp, run)) return tr(COMPACTING_LABEL)
  const n = lamp === 'idle' ? backgroundJobs(run) : 0
  if (n > 0) return backgroundLabel(n)
  const why = lamp === 'blocked' ? blockedReasonOf(run) : ''
  return why ? t('等待回應：{reason}', { reason: why }) : tr(LAMP_LABEL[lamp])
}
