/**
 * 手機版重新登入 claude 身分（#838，使用者 2026-10-04）：登入 pane 上的「打開登入網站」與 code 輸入框的判斷。
 * daemon 從 `claude auth login` 的畫面取網址、確認還在等 code 才把字打進去；這裡只決定畫面上顯示什麼。
 */
import type { LoginStatus } from '../api'

/** 跟 daemon（`login_assist::valid_code`）同一套：OAuth code 會有的字元，最長 1024。前端先擋，daemon 仍會再驗。 */
export function validLoginCode(code: string): boolean {
  const c = code.trim()
  return c.length > 0 && c.length <= 1024 && /^[A-Za-z0-9_.~#:/+=%-]+$/.test(c)
}

export type LoginPhase =
  /** CLI 還沒印出網址。 */
  | 'starting'
  /** 有網址，可以去登入；提示還沒出現（或 CLI 還在處理）。 */
  | 'url'
  /** 畫面正在等貼 code。 */
  | 'waiting_code'
  /** code 已送出，等 CLI 回應。 */
  | 'submitted'
  /** CLI 說登入失敗。 */
  | 'failed'
  /** pane 已收掉（CLI 結束）。 */
  | 'ended'

export function loginPhase(status: LoginStatus | null, ended: boolean): LoginPhase {
  if (ended || !status) return 'ended'
  if (status.failure) return 'failed'
  if (status.code_sent) return 'submitted'
  if (status.awaiting_code) return 'waiting_code'
  return status.url ? 'url' : 'starting'
}

/** 送出鈕要不要能按：畫面在等 code、格式對、不在送出中。 */
export function canSubmitCode(phase: LoginPhase, code: string, sending: boolean): boolean {
  return phase === 'waiting_code' && !sending && validLoginCode(code)
}

/** 輸入框下面那一句。 */
export function loginHint(phase: LoginPhase, code: string): string {
  switch (phase) {
    case 'starting':
      return '等 CLI 印出登入網址…'
    case 'url':
      return '去登入網站完成授權，網站會給你一串 code；CLI 準備好收 code 後這裡就能貼。'
    case 'waiting_code':
      return code.trim() && !validLoginCode(code) ? 'code 只會有英數字與 _ . ~ # : / + = % -，看一下有沒有多貼到別的字' : '把網站給的 code 貼在這裡，按送出。'
    case 'submitted':
      return '已送出，等 CLI 回應…'
    case 'failed':
      return '登入失敗：重新按「登入」再來一次。'
    case 'ended':
      return '登入程序已結束。'
  }
}
