import type { Run } from '../api/types'

/**
 * 這個 run 現在為什麼停在「等待回應」（daemon 的 `run.blocked_reason.text`，例如「codex 更新提示等待選擇」）。
 * 只有 run 還活著、而且**現在**真的是 blocked 才給字：herdr 自己報了別的狀態、run 結束後，就算收到的資料裡還帶著舊原因也不顯示。
 * 沒有原因（舊 daemon 沒這個欄位、daemon 不知道為什麼）給空字串，呼叫端照舊只顯示「等待回應」。
 */
export function blockedReasonOf(run: Run | null | undefined): string {
  if (!run || run.agent_status !== 'blocked' || run.state === 'exited' || run.state === 'stopped') return ''
  return run.blocked_reason?.text ?? ''
}
