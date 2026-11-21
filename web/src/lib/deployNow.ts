import { ApiError } from '../api/types'
import { t } from './i18n'

export interface DeployCommit {
  sha: string
  subject: string
}

export interface DeployRunning {
  kind: string
  sha?: string
  resource?: string
  owner?: string
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
  error: string | null
}

export function deployVisible(s: DeployStatus | null): boolean {
  if (!s) return false
  return s.running !== null || (s.code_changed && s.behind > 0)
}

export function runningText(r: DeployRunning): string {
  switch (r.kind) {
    case 'requested':
      return t('部署中{sha}：建置中或等換版窗口', { sha: r.sha ? t('（{sha}）', { sha: r.sha.slice(0, 8) }) : '' })
    case 'lease':
      return t('{owner}正握著 {resource} 窗口（重建或換版進行中）', { owner: r.owner || t('有人'), resource: r.resource ?? '' })
    default:
      return t('有部署在跑')
  }
}

export interface DeployNotice {
  level: 'info' | 'error'
  text: string
}

export function deployNowNotice(result: { short?: string; log_path?: string } | null, error: unknown): DeployNotice {
  if (result) {
    return {
      level: 'info',
      text: t('已送出部署 {short}。左上角會顯示「部署中」，換好後自動消失；等太久會跳出等待提示。', { short: result.short ?? '' }),
    }
  }
  if (error instanceof ApiError) {
    const b = error.body as Record<string, unknown>
    const msg = typeof b.message === 'string' && b.message ? b.message : error.message
    if (error.status === 409 && b.reason === 'deploy_in_progress') return { level: 'error', text: t('沒有再開一趟：{msg}', { msg }) }
    if (error.status === 409 && b.reason === 'nothing_to_deploy') return { level: 'info', text: t('不用部署：{msg}', { msg }) }
    if (error.status === 409 && b.reason === 'target_older_than_live') return { level: 'error', text: t('部署已拒絕：{msg}', { msg }) }
    if (error.status === 403) return { level: 'error', text: t('不能從這裡部署：{msg}', { msg }) }
    return { level: 'error', text: t('立即部署沒有開始：{msg}', { msg }) }
  }
  return { level: 'error', text: t('立即部署沒有開始：{msg}', { msg: error instanceof Error ? error.message : String(error) }) }
}
