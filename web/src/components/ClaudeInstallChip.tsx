import { useState } from 'react'
import * as api from '../api'
import { claudeInstallPlan } from '../lib/updateBatch'
import { reviewErrText } from '../lib/claudeReviewErr'
import { CLI_UPDATE_PHASE_LABEL } from '../store/cliUpdate'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import type { DialogControl } from './MergedUpdateChip'

/** Claude fleet 尚未統一到共同目標版的持續提示；使用者確認後才逐台執行 `claude install <version>`。 */
export function ClaudeInstallChip({ control }: { control?: DialogControl } = {}) {
  const planKey = useStore((s) => {
    const plan = claudeInstallPlan(s.upstreamUpdates.claude)
    return plan ? JSON.stringify(plan) : ''
  })
  const activeKey = useStore((s) => JSON.stringify(s.cliUpdates.filter((item) => item.kind === 'claude')))
  const install = useStore((s) => s.installClaudeUpdates)
  const notify = useStore((s) => s.notify)
  const [localOpen, setLocalOpen] = useState(false)
  const confirming = control ? control.open : localOpen
  const setConfirming = (open: boolean) => (control ? !open && control.close() : setLocalOpen(open))
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)

  const active = JSON.parse(activeKey) as { id: string; host: string; phase: keyof typeof CLI_UPDATE_PHASE_LABEL; from: string | null; to: string | null }[]
  if (!planKey) {
    if (control || active.length === 0) return null
    const current = active[0]
    const label = `${current.host} 的 claude ${CLI_UPDATE_PHASE_LABEL[current.phase]}…`
    return (
      <button type="button" className="quota-update install running" disabled title={label} aria-label={label}>
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">…</span>
      </button>
    )
  }

  const plan = JSON.parse(planKey) as NonNullable<ReturnType<typeof claudeInstallPlan>>
  const activeHosts = new Set(active.map((item) => item.host))
  const toInstall = plan.installHosts.filter((host) => !activeHosts.has(host))
  const hostRows = plan.hosts.map((host) => `${host.host}：${host.installedVersion ?? `讀取失敗${host.error ? `（${host.error}）` : ''}`} → ${plan.target}`)
  const hostForDetails = plan.installHosts[0] ?? plan.hosts[0]?.host ?? 'local'
  const from = plan.hosts.find((host) => host.host === hostForDetails)?.installedVersion ?? null
  const label = active.length > 0
    ? `claude 共同目標 ${plan.target}，安裝中 ${active.map((item) => `${item.host} ${CLI_UPDATE_PHASE_LABEL[item.phase]}`).join('、')}；${hostRows.join('；')}`
    : `claude 需安裝到 ${plan.target}；${hostRows.join('；')}。確認後安裝 ${toInstall.length} 台`

  return (
    <>
      {control ? null : (
        <button
          type="button"
          className={`quota-update install${active.length ? ' running' : ''}`}
          title={label}
          aria-label={label}
          disabled={toInstall.length === 0}
          onClick={() => setConfirming(true)}
        >
          <span aria-hidden="true"><UpgradeIcon /></span>
          <span className="quota-update-n" aria-hidden="true">{active.length ? '…' : toInstall.length}</span>
        </button>
      )}
      <ConfirmDialog
        open={confirming}
        title={`將 claude 安裝到 ${plan.target}？`}
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind="claude" host={hostForDetails} from={from} to={plan.target} />
                <AgmReviewBox kind="claude" host={hostForDetails} from={from} to={plan.target} refreshKey={reviewKey} />
              </div>
            ) : null}
            <p>按下確認後，才會在以下落後或讀不到版本的主機執行 <code>claude install {plan.target}</code>。每台都會安裝同一個目標版；安裝完成後只顯示「已安裝，重啟套用」，不會自動重啟。</p>
            <ul className="confirm-list">
              {plan.hosts.map((host) => (
                <li key={host.host}>
                  {host.host}：{host.installedVersion ?? `讀取失敗${host.error ? `（${host.error}）` : ''}`} → {plan.target}
                  {activeHosts.has(host.host) ? `（${CLI_UPDATE_PHASE_LABEL[active.find((item) => item.host === host.host)!.phase]}）` : ''}
                </li>
              ))}
            </ul>
            <p className="confirm-note">已在目標版的主機不重裝。各台安裝狀態會留在 header，直到全部到版。</p>
          </>
        }
        confirmLabel={toInstall.length > 0 ? `安裝到 ${toInstall.length} 台` : '安裝中'}
        secondaryLabel={asking ? '派工中…' : '請 AGM 解析'}
        secondaryDisabled={asking || toInstall.length === 0}
        onSecondary={() => {
          setAsking(true)
          void api.requestUpdateReview({ kind: 'claude', host: hostForDetails, from, to: plan.target })
            .then((r) => {
              setReviewKey((n) => n + 1)
              notify('info', r.duplicate
                ? `claude ${r.version} 已經派給 ${r.target_bot_name || 'AGM'} 解析過了，結論會回到這裡`
                : `已請 ${r.target_bot_name} 解析 claude ${r.version} 的 changelog，結論會回到這裡`)
            })
            .catch((e: unknown) => notify('error', `派不出去：${reviewErrText(e)}`))
            .finally(() => setAsking(false))
        }}
        width={480}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false)
          void install(toInstall, plan.target)
        }}
      />
    </>
  )
}
