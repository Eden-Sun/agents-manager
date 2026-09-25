import { ApiError } from '../api/types'

/**
 * `POST /api/claude-update/review`（「請 AGM 解析」）失敗時使用者看到的那一句。
 * daemon 的 409 帶 `reason`（`no_target`／`no_task_file`／`no_version`）與給使用者看的 `message`（API.md）；
 * `ApiError.message` 取的是 `reason`，直接顯示只剩 `派不出去：no_target`，daemon 寫好的「到 AGM 設定裡指定協調者」被丟掉。
 */
export function reviewErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 404 || e.status === 405) {
      return `這顆 daemon 還沒有 /api/claude-update/review（HTTP ${e.status}），二進位比前端舊；要重建並重啟 daemon 才會有這支 API。`
    }
    const human = e.body.message
    if (typeof human === 'string' && human.trim()) return human.trim()
    if (e.body.reason === 'no_version') {
      return `這台主機還讀不到 ${typeof e.body.kind === 'string' ? e.body.kind : 'claude'} 的版本（等它回報之後再按一次）。`
    }
  }
  return e instanceof Error ? e.message : String(e)
}
