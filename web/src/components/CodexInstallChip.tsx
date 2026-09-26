import { useState } from 'react'
import * as api from '../api'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { codexInstallPlan } from '../lib/updateBatch'
import { reviewErrText } from '../lib/claudeReviewErr'
import { updateRange } from '../lib/updateRange'
import { CLI_UPDATE_PHASE_LABEL } from '../store/cliUpdate'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import type { DialogControl } from './MergedUpdateChip'

/** 確認框裡照實寫出來的安裝指令：跟 daemon `cli_update::CODEX_INSTALL` 同一句（daemon 不收呼叫端傳的指令，這裡只是給人看）。 */
const INSTALL_CMD = 'curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh'

/**
 * header 的「codex 有新版、還沒裝」（SPEC §6.9，使用者 2026-09-25：「codex 的 upgrade 也和 claude 用一樣的方式出現在 header」）。
 * 跟 claude 那顆同一個樣子（⌃⌃ N，警示色）、同一種確認框（左 changelog、右 AGM 解析、下面列會重啟／會跳過的），
 * 確認後 daemon 在那台跑官方安裝指令 → 驗 `codex --version` → 接著一鍵重啟那台閒置的 codex。claude 也有更新時兩顆並排（UI-DECISIONS）。
 */
export function CodexInstallChip({ control }: { control?: DialogControl } = {}) {
  // 各 selector 回純值（見 UpdateQuotaChip：回新物件會 getSnapshot should be cached）。
  const planKey = useStore((s) => {
    const p = codexInstallPlan(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null, (b) => projectHostName(s, b.project_id))
    return p ? JSON.stringify(p) : ''
  })
  const cli = useStore((s) => s.cliUpdate)
  const install = useStore((s) => s.installCodexUpdate)
  const notify = useStore((s) => s.notify)
  const [localOpen, setLocalOpen] = useState(false)
  // 手機合成的那顆（`MergedUpdateChip`）代為開框時，這裡只畫確認框。
  const confirming = control ? control.open : localOpen
  const setConfirming = (v: boolean) => (control ? !v && control.close() : setLocalOpen(v))
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)

  if (cli && !control) {
    const label = `${cli.host} 的 codex ${CLI_UPDATE_PHASE_LABEL[cli.phase]}${cli.from ? `（${cli.from}${cli.to ? ` → ${cli.to}` : ''}）` : ''}…`
    return (
      <button type="button" className="quota-update install running" disabled title={label} aria-label={label}>
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">…</span>
      </button>
    )
  }
  if (!planKey) return null
  const plan = JSON.parse(planKey) as NonNullable<ReturnType<typeof codexInstallPlan>>
  const range = updateRange('codex', plan.notice, null)
  const version = range.to ? ` ${range.to}` : ''
  const label = `codex 有新版${version}，還沒裝（${plan.host}，${plan.installCount} 顆 Bot）· 點一下安裝並重啟閒置的 codex`

  return (
    <>
      {control ? null : <button
        type="button"
        className="quota-update install"
        title={`${label}${plan.ready.length ? `：\n${plan.ready.map((b) => b.name).join('、')}` : ''}`}
        aria-label={label}
        onClick={() => setConfirming(true)}
      >
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">{plan.installCount}</span>
      </button>}
      <ConfirmDialog
        open={confirming}
        title={`安裝 codex${version} 並重啟？`}
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind="codex" host={plan.host} from={range.from} to={range.to} />
                <AgmReviewBox kind="codex" host={plan.host} from={range.from} to={range.to} refreshKey={reviewKey} />
              </div>
            ) : null}
            <p>
              會在 <strong>{plan.host}</strong> 跑 codex 官方的安裝指令 <code>{INSTALL_CMD}</code>，確認{' '}
              <code>codex --version</code> 真的升上去之後，
              {plan.ready.length > 0 ? (
                <>
                  以下 <strong>{plan.ready.length}</strong> 顆會結束目前的 agent，再用同一個 session <code>--resume</code> 接回來：
                </>
              ) : (
                <>目前沒有閒置的 codex 可以重啟（裝好後它們閒下來再按 ⌃⌃）。</>
              )}
            </p>
            {plan.ready.length > 0 ? (
              <ul className="confirm-list">
                {plan.ready.map((b) => (
                  <li key={b.botId}>{b.name}</li>
                ))}
              </ul>
            ) : null}
            {plan.busy.length > 0 ? (
              <>
                <p className="confirm-note">另外 {plan.busy.length} 顆會先跳過（新版照樣裝好，之後再按 ⌃⌃ 重啟）：</p>
                <ul className="confirm-list dim">
                  {plan.busy.map((b) => (
                    <li key={b.botId}>
                      {b.name}（{b.why}）
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            <p className="confirm-note">安裝失敗、裝完版本沒變{version ? `或沒到${version}` : ''}，就一顆都不重啟，並告訴你原因。</p>
          </>
        }
        confirmLabel={plan.ready.length > 0 ? `安裝並重啟 ${plan.ready.length} 顆` : '只安裝'}
        secondaryLabel={asking ? '派工中…' : '請 AGM 解析'}
        secondaryDisabled={asking}
        onSecondary={() => {
          setAsking(true)
          void api
            .requestUpdateReview({ kind: 'codex', host: plan.host, from: range.from, to: range.to })
            .then((r) => {
              setReviewKey((n) => n + 1)
              notify(
                'info',
                r.duplicate
                  ? `codex ${r.version} 已經派給 ${r.target_bot_name || 'AGM'} 解析過了，結論會回到這裡`
                  : `已請 ${r.target_bot_name} 解析 codex ${r.version} 的 changelog，結論會回到這裡`,
              )
            })
            .catch((e: unknown) => notify('error', `派不出去：${reviewErrText(e)}`))
            .finally(() => setAsking(false))
        }}
        width={440}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false)
          void install(plan.host, range.to ?? '')
        }}
      />
    </>
  )
}
