import { ApiError } from '../api/types'
import { t } from './i18n'
import { humanMessage, plainErrMsg } from './errText'

export function reviewErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 404 || e.status === 405) {
      return t('這顆 daemon 還沒有 /api/claude-update/review（HTTP {status}），二進位比前端舊；要重建並重啟 daemon 才會有這支 API。', { status: e.status })
    }
    const human = humanMessage(e)
    if (human) return human
    if (e.body.reason === 'no_version') {
      return t('這台主機還讀不到 {kind} 的版本（等它回報之後再按一次）。', { kind: typeof e.body.kind === 'string' ? e.body.kind : 'claude' })
    }
  }
  return plainErrMsg(e)
}
