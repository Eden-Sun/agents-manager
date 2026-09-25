/**
 * 左上角「立即部署」的判斷與結果文字（使用者 2026-09-25）。純函式，元件只負責畫。
 *
 * 資料是 `GET /api/deploy/status`：線上 binary（`live_sha`）落後 origin/main 幾個 commit、有沒有會進 binary 的差異
 * （路徑同 `daemon-update-kick.sh` 的 build-inputs；docs-only 不算）、現在有沒有部署在跑。
 */
import { ApiError } from '../api/types'

export interface DeployCommit {
  sha: string
  subject: string
}

export interface DeployRunning {
  /** `requested`＝按過、kick 還沒派出去（或在等安全窗口）；`lease`＝有人握著 rebuild／restart 窗口；`assignment`＝更新交辦還沒結案。 */
  kind: string
  sha?: string
  resource?: string
  owner?: string
  status?: string
  client_request_id?: string
}

export interface DeployStatus {
  live_sha: string
  target_sha: string
  target_short: string
  behind: number
  code_commits: number
  code_changed: boolean
  commits: DeployCommit[]
  commits_truncated: boolean
  running: DeployRunning | null
  working: { bot_id: string; name: string }[]
  log_path: string
  kick_ready: boolean
  /** daemon 說不出落後多少的原因（sha 是 unknown、讀不到 origin/main）。 */
  error: string | null
}

/** 按鈕要不要出現：有程式碼差異、或正在部署（讓使用者看得到「部署中」而不是按鈕突然消失）。 */
export function deployVisible(s: DeployStatus | null): boolean {
  if (!s) return false
  return s.running !== null || (s.code_changed && s.behind > 0)
}

/** 正在跑的是哪一種，寫成一句人話（chip 的提示與確認框用）。 */
export function runningText(r: DeployRunning): string {
  switch (r.kind) {
    case 'requested':
      return `已按下立即部署${r.sha ? `（${r.sha.slice(0, 8)}）` : ''}，等例行更新接手或等安全窗口`
    case 'lease':
      return `${r.owner || '有人'}正握著 ${r.resource ?? ''} 窗口（重建或換版進行中）`
    case 'assignment':
      return `更新交辦 ${r.client_request_id ?? ''} 還沒結案（${r.status ?? ''}）`
    default:
      return '有部署在跑'
  }
}

export interface DeployNotice {
  level: 'info' | 'error'
  text: string
}

/** `POST /api/deploy/now` 的結果 → 一則通知。409／503 講清楚是哪一種，不要只說「失敗」。 */
export function deployNowNotice(result: { short?: string; log_path?: string } | null, error: unknown): DeployNotice {
  if (result) {
    return {
      level: 'info',
      text: `已開始立即部署 ${result.short ?? ''}：照例行流程建置、整樹測試、沒人 working 才換 binary。進度看 ${result.log_path ?? 'daemon-update.log'}`,
    }
  }
  if (error instanceof ApiError) {
    const b = error.body as Record<string, unknown>
    const msg = typeof b.message === 'string' && b.message ? b.message : error.message
    if (error.status === 409 && b.reason === 'deploy_in_progress') return { level: 'error', text: `沒有再開一趟：${msg}` }
    if (error.status === 409 && b.reason === 'nothing_to_deploy') return { level: 'info', text: `不用部署：${msg}` }
    if (error.status === 403) return { level: 'error', text: `不能從這裡部署：${msg}` }
    return { level: 'error', text: `立即部署沒有開始：${msg}` }
  }
  return { level: 'error', text: `立即部署沒有開始：${error instanceof Error ? error.message : String(error)}` }
}
