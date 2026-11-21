import type { Run } from '../api/types'

export function blockedReasonOf(run: Run | null | undefined): string {
  if (!run || run.agent_status !== 'blocked' || run.state === 'exited' || run.state === 'stopped') return ''
  return run.blocked_reason?.text ?? ''
}
