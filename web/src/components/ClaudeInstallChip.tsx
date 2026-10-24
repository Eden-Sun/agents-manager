import { useState } from 'react'
import * as api from '../api'
import { claudeInstallPlan } from '../lib/updateBatch'
import { reviewErrText } from '../lib/claudeReviewErr'
import { CLI_UPDATE_PHASE_LABEL } from '../store/cliUpdate'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import type { DialogControl } from './MergedUpdateChip'

/** Claude fleet 尚未統一到共同目標版的持續提示；使用者確認後才逐台執行 `claude install <version>`。 */
export function ClaudeInstallChip({ control }: { control?: DialogControl } = {}) {
  const t = useT()
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
    const label = t('{host} 的 claude {phase}…', { host: current.host, phase: t.tr(CLI_UPDATE_PHASE_LABEL[current.phase]) })
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
  const hostRow = (host: (typeof plan.hosts)[number]) =>
    t('{host}：{ver} → {target}', {
      host: host.host,
      ver: host.installedVersion ?? (host.error ? t('讀取失敗（{error}）', { error: host.error }) : t('讀取失敗')),
      target: plan.target,
    })
  const hostRows = plan.hosts.map(hostRow)
  const hostForDetails = plan.installHosts[0] ?? plan.hosts[0]?.host ?? 'local'
  const from = plan.hosts.find((host) => host.host === hostForDetails)?.installedVersion ?? null
  const label = active.length > 0
    ? t('claude 共同目標 {target}，安裝中 {progress}；{rows}', {
        target: plan.target,
        progress: active.map((item) => t('{host} {phase}', { host: item.host, phase: t.tr(CLI_UPDATE_PHASE_LABEL[item.phase]) })).join(t('、')),
        rows: hostRows.join(t('；')),
      })
    : t('claude 需安裝到 {target}；{rows}。確認後安裝 {n} 台', { target: plan.target, rows: hostRows.join(t('；')), n: toInstall.length })

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
        title={t('將 claude 安裝到 {target}？', { target: plan.target })}
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind="claude" host={hostForDetails} from={from} to={plan.target} autoTranslate />
                <AgmReviewBox kind="claude" host={hostForDetails} from={from} to={plan.target} refreshKey={reviewKey} />
              </div>
            ) : null}
            <p>{t.x('按下確認後，才會在以下落後或讀不到版本的主機執行 {cmd}。每台都會安裝同一個目標版；安裝完成後只顯示「已安裝，重啟套用」，不會自動重啟。', { cmd: <code>claude install {plan.target}</code> })}</p>
            <ul className="confirm-list">
              {plan.hosts.map((host) => (
                <li key={host.host}>
                  {hostRow(host)}
                  {activeHosts.has(host.host) ? t('（{phase}）', { phase: t.tr(CLI_UPDATE_PHASE_LABEL[active.find((item) => item.host === host.host)!.phase]) }) : ''}
                </li>
              ))}
            </ul>
            <p className="confirm-note">{t('已在目標版的主機不重裝。各台安裝狀態會留在 header，直到全部到版。')}</p>
          </>
        }
        confirmLabel={toInstall.length > 0 ? t('安裝到 {n} 台', { n: toInstall.length }) : t('安裝中')}
        secondaryLabel={asking ? t('派工中…') : t('請 AGM 解析')}
        secondaryDisabled={asking || toInstall.length === 0}
        onSecondary={() => {
          setAsking(true)
          void api.requestUpdateReview({ kind: 'claude', host: hostForDetails, from, to: plan.target })
            .then((r) => {
              setReviewKey((n) => n + 1)
              notify('info', r.duplicate
                ? t('claude {version} 已經派給 {bot} 解析過了，結論會回到這裡', { version: r.version, bot: r.target_bot_name || 'AGM' })
                : t('已請 {bot} 解析 claude {version} 的 changelog，結論會回到這裡', { bot: r.target_bot_name, version: r.version }))
            })
            .catch((e: unknown) => notify('error', t('派不出去：{reason}', { reason: reviewErrText(e) })))
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
