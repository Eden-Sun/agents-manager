/**
 * 部署等換版窗口的前端狀態（SPEC §18.10）。自己一個小 store（同 `herdrUpdate.ts`）：同時只會有一次部署，
 * 跟 `useStore` 的快照替換無關。來源：`/api/state` 的 `deploy_wait`（重整對帳）與 WS `deploy_wait`。
 */
import { create } from 'zustand'
import { toDeployWait, type DeployWait } from '../api/deployWait'

export const useDeployWait = create<{ wait: DeployWait | null }>(() => ({ wait: null }))

/** 快照：欄位不在（舊 daemon）＝不知道，不動；在而且是 null＝現在沒有。 */
export function applyDeployWaitSnapshot(raw: unknown) {
  if (raw === undefined) return
  useDeployWait.setState({ wait: toDeployWait(raw) })
}

export function setDeployWait(w: DeployWait | null) {
  useDeployWait.setState({ wait: w })
}

/** WS `deploy_wait`：更新狀態，回傳要跳的通知（第一次通知、拿到窗口、換好、放棄）；擋的人換了只更新 header，不跳。 */
export function onDeployWaitFrame(data: unknown): { kind: 'info' | 'error'; text: string } | null {
  const d = typeof data === 'object' && data !== null ? (data as Record<string, unknown>) : {}
  const w = toDeployWait(d.wait)
  if (!w) return null
  const prev = useDeployWait.getState().wait
  useDeployWait.setState({ wait: w })
  if (d.first === true) return { kind: 'error', text: w.summary }
  if (prev?.id === w.id && prev.phase === w.phase) return null
  if (w.phase === 'swapping') return { kind: 'info', text: w.summary }
  if (w.phase === 'done') return { kind: 'info', text: w.summary }
  if (w.phase === 'abandoned') return { kind: 'error', text: w.summary }
  return null
}
