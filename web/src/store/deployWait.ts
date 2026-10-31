/**
 * 部署等換版窗口的前端狀態（SPEC §18.10）。自己一個小 store（同 `herdrUpdate.ts`）：同時只會有一次部署，
 * 跟 `useStore` 的快照替換無關。來源：`/api/state` 的 `deploy_wait`（重整對帳）與 WS `deploy_wait`。
 */
import { create } from 'zustand'
import { BLOCKER_WHY, toDeployWait, type DeployWait } from '../api/deployWait'
import { t, tr } from '../lib/i18n'

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
export function deployWaitNotice(w: DeployWait, now = Date.now()): string {
  const blockers = w.blockers.map((b) => `${b.name}（${tr(BLOCKER_WHY[b.why] ?? b.why)}）`).join('、')
  const since = Date.parse(w.since)
  const waited = Math.max(0, Math.floor((Number.isFinite(since) ? now - since : w.waited_secs * 1000) / 60_000))
  const intro = blockers
    ? t('部署 {commit} 已等待 {mins} 分鐘，{blockers} 尚未就緒。', { commit: w.commit.slice(0, 8), mins: waited, blockers })
    : t('部署 {commit} 已等待 {mins} 分鐘。', { commit: w.commit.slice(0, 8), mins: waited })
  const deadline = w.escalates_at ? Date.parse(w.escalates_at) : NaN
  const when = Number.isFinite(deadline) ? (() => {
    const local = new Date(deadline + 8 * 60 * 60 * 1000)
    const clock = `${String(local.getUTCHours()).padStart(2, '0')}:${String(local.getUTCMinutes()).padStart(2, '0')}`
    const mins = Math.max(0, Math.ceil((deadline - now) / 60_000))
    return deadline > now
      ? t('預計 {time}（UTC+8，{mins} 分鐘後）自動放寬。', { time: clock, mins })
      : t('已到 {time}（UTC+8），等待系統處理。', { time: clock })
  })() : ''
  return [intro, when, t('可在頁首選擇「現在換版」或「先等」。')].filter(Boolean).join(' ')
}

export function onDeployWaitFrame(data: unknown): { kind: 'info' | 'error'; text: string; topCenter?: boolean } | null {
  const d = typeof data === 'object' && data !== null ? (data as Record<string, unknown>) : {}
  const w = toDeployWait(d.wait)
  if (!w) return null
  const prev = useDeployWait.getState().wait
  useDeployWait.setState({ wait: w })
  if (d.first === true) return { kind: 'error', text: deployWaitNotice(w), topCenter: true }
  if (prev?.id === w.id && prev.phase === w.phase) return null
  if (w.phase === 'swapping') return { kind: 'info', text: w.summary }
  if (w.phase === 'done') return { kind: 'info', text: w.summary }
  if (w.phase === 'abandoned') return { kind: 'error', text: w.summary }
  return null
}
