import type { Lamp, Run } from '../api/types'
import { backgroundJobs, backgroundLabel } from '../lib/backgroundJobs'
import { blockedReasonOf } from '../lib/blockedReason'
import { t, tr } from '../lib/i18n'

// This identity tag stays local so the split initial-ui chunk does not call into lib/i18n while modules initialize.
const msg = <K extends string>(key: K): K => key

/** SPEC §2.2 colour rules. */
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

/** 壓縮中的字（issue #1770）：跟「執行中」分開，讓人知道是在壓縮對話、不是在做事。 */
export const COMPACTING_LABEL = msg('壓縮中…')

/** 壓縮中（daemon 的 `run.compacting_since`，#1770）：燈號是執行中或閒置時才算；斷線、等待回應不蓋掉。 */
export function isCompacting(lamp: Lamp, run: Run | null | undefined): boolean {
  return (lamp === 'working' || lamp === 'idle') && Boolean(run?.compacting_since)
}

/**
 * 這顆 bot 現在的狀態字（#714）：燈號是 idle、而且回合結束後背景還有工作在跑時是「背景執行中（N）」，不然是 `LAMP_LABEL`。
 * 側欄、標題燈、BotSwitcher、群組成員、手機主力晶片都從這裡拿文案，不各寫一份「閒置」。
 * 只有 idle 才改：working／blocked／斷線時燈號已經說清楚了（斷線時畫面讀不到，背景數字也不可信）。
 * 壓縮中（#1770）排在最前面：壓縮時背景數字照樣有，但這一刻要說的是「壓縮中」。
 */
export function botStateLabel(lamp: Lamp, run: Run | null | undefined): string {
  if (isCompacting(lamp, run)) return tr(COMPACTING_LABEL)
  const n = lamp === 'idle' ? backgroundJobs(run) : 0
  if (n > 0) return backgroundLabel(n)
  // blocked 帶原因（daemon 的 `run.blocked_reason`），原因文字不翻譯。
  const why = lamp === 'blocked' ? blockedReasonOf(run) : ''
  return why ? t('等待回應：{reason}', { reason: why }) : tr(LAMP_LABEL[lamp])
}
