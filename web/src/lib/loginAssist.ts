import type { LoginStatus } from '../api'
import { t } from './i18n'

export function validLoginCode(code: string): boolean {
  const c = code.trim()
  return c.length > 0 && c.length <= 1024 && /^[A-Za-z0-9_.~#:/+=%-]+$/.test(c)
}

export type LoginPhase =
  | 'starting'
  | 'url'
  | 'device'
  | 'waiting_code'
  | 'submitted'
  | 'failed'
  | 'ended'

export function loginPhase(status: LoginStatus | null, ended: boolean): LoginPhase {
  if (ended || !status) return 'ended'
  if (status.failure) return 'failed'
  if (status.flow === 'device') return status.url && status.device_code ? 'device' : 'starting'
  if (status.code_sent) return 'submitted'
  if (status.awaiting_code) return 'waiting_code'
  return status.url ? 'url' : 'starting'
}

export function canSubmitCode(phase: LoginPhase, code: string, sending: boolean): boolean {
  return phase === 'waiting_code' && !sending && validLoginCode(code)
}

export function loginHint(phase: LoginPhase, code: string): string {
  switch (phase) {
    case 'starting':
      return t('等 CLI 印出登入網址…')
    case 'device':
      return t('到登入網站輸入上面的一次性碼，完成後這裡會自動更新。不用貼回終端。')
    case 'url':
      return t('去登入網站完成授權，網站會給你一串 code；CLI 準備好收 code 後這裡就能貼。')
    case 'waiting_code':
      return code.trim() && !validLoginCode(code)
        ? t('code 只會有英數字與 _ . ~ # : / + = % -，看一下有沒有多貼到別的字')
        : t('把網站給的 code 貼在這裡，按送出。')
    case 'submitted':
      return t('已送出，等 CLI 回應…')
    case 'failed':
      return t('登入失敗：重新按「登入」再來一次。')
    case 'ended':
      return t('登入程序已結束。')
  }
}
