import { useState } from 'react'
import { ADD_PROJECT_EVENT, KIND_STATE_LABEL, type OnbKindRow } from '../lib/onboarding'
import { useOnboarding } from '../lib/useOnboarding'
import { installBusyKey, installKindOnHost } from '../lib/onboardingActions'
import { AGY_LOGIN_GUIDE, cliLoginCommand } from '../lib/quotaLogin'
import { useStore } from '../store/store'
import { AgyInstallButton } from './AgyInstall'
import { KindTag } from './KindTag'
import { KIND_LABEL } from './kindMeta'
import { QuotaLoginShell } from './QuotaLoginShell'
import './onboarding.css'

function KindRowView({ row }: { row: OnbKindRow }) {
  const installBusy = useStore((s) => s.busy[installBusyKey('local', row.kind)] === true)
  const label = KIND_LABEL[row.kind]
  return (
    <li className={`onb-row is-${row.state}`} data-kind={row.kind} data-state={row.state}>
      <span className="onb-mark" aria-hidden="true">
        {row.state === 'ok' ? '✓' : row.state === 'unknown' ? '?' : row.state === 'out' ? '!' : '✗'}
      </span>
      <KindTag kind={row.kind} />
      <span className="onb-name">{label}</span>
      <span className="onb-state">
        {KIND_STATE_LABEL[row.state]}
        {row.state !== 'missing' && row.version ? ` ${row.version}` : ''}
        {row.account ? `（${row.account}）` : ''}
      </span>
      {row.state === 'missing' ? (
        row.kind === 'agy' ? (
          // agy 由 daemon 讀官方 manifest 裝（先確認），沿用既有按鈕。
          <div className="onb-act">
            <AgyInstallButton host="local" />
          </div>
        ) : (
          <button
            type="button"
            className="btn primary onb-act"
            disabled={installBusy}
            title={`在本機開 shell 執行 ${label} 的官方安裝指令`}
            onClick={() => void installKindOnHost('local', row.kind)}
          >
            {installBusy ? '安裝中…' : '安裝'}
          </button>
        )
      ) : row.state === 'out' ? (
        <div className="onb-act">
          <QuotaLoginShell host="local" hostLabel="本機" kind={row.kind} command={cliLoginCommand(row.kind, {})} guide={row.kind === 'agy' ? AGY_LOGIN_GUIDE : undefined} />
        </div>
      ) : null}
    </li>
  )
}

/**
 * 環境準備清單（issue #1931）：herdr、四種 CLI 的安裝／登入現況＋按鈕，以及接下來「新增 Project → 新增 Bot」的引導。
 * 主畫面（`OnboardingBanner`）與環境設定的「環境準備」分頁共用這一份。安裝 agy 用既有的確認框（`AgyInstallButton` 那一套），其他 CLI 由 daemon 開臨時 shell 裝。
 */
export function OnboardingChecklist() {
  const m = useOnboarding()
  const projects = useStore((s) => s.projects)
  const requestOpenBotSheet = useStore((s) => s.requestOpenBotSheet)
  const refreshTools = useStore((s) => s.refreshTools)
  const refreshState = useStore((s) => s.refreshState)
  const toolsBusy = useStore((s) => s.busy['tools:local'] === true)
  return (
    <div className="onb" role="group" aria-label="環境準備">
      <ul className="onb-list">
        <li className={`onb-row is-${m.herdrConnected ? 'ok' : 'missing'}`} data-kind="herdr" data-state={m.herdrConnected ? 'ok' : 'missing'}>
          <span className="onb-mark" aria-hidden="true">
            {m.herdrConnected ? '✓' : '✗'}
          </span>
          <span className="onb-name">herdr</span>
          <span className="onb-state">{m.herdrConnected ? '已連線' : '連不上——請確認 herdr 已安裝並在背景執行'}</span>
          {m.herdrConnected ? null : (
            <button type="button" className="btn onb-act" onClick={() => void refreshState()}>
              重試
            </button>
          )}
        </li>
        {m.rows.map((r) => (
          <KindRowView key={r.kind} row={r} />
        ))}
      </ul>
      <div className="onb-foot">
        <button type="button" className="btn" disabled={toolsBusy} title="重新問本機的 CLI 現在裝了什麼、登入了誰" onClick={() => void refreshTools('local')}>
          {toolsBusy ? '偵測中…' : '重新偵測'}
        </button>
        <ol className="onb-steps" aria-label="接下來">
          <li className={m.anyReady ? 'is-done' : 'is-now'}>安裝並登入至少一種 CLI</li>
          <li className={projects.length > 0 ? 'is-done' : m.anyReady ? 'is-now' : ''}>
            <button type="button" className="btn" disabled={!m.anyReady} onClick={() => window.dispatchEvent(new Event(ADD_PROJECT_EVENT))}>
              新增 Project
            </button>
          </li>
          <li className={projects.length > 0 && m.anyReady ? 'is-now' : ''}>
            <button type="button" className="btn primary" disabled={!m.anyReady || projects.length === 0} onClick={() => requestOpenBotSheet(projects[0].id)}>
              新增 Bot
            </button>
          </li>
        </ol>
      </div>
      {m.needed ? null : <p className="onb-all hint">環境已就緒。</p>}
    </div>
  )
}

/**
 * 主畫面的環境準備提示：沒有 Project、或本機沒有任何可用（已裝且已登入／問不到）的 kind、或 herdr 連不上時顯示；
 * 都好了自動收起。選著 Bot／群組／shell 時收成一行（點開才展開），空畫面時整張攤開。
 */
export function OnboardingBanner({ compact }: { compact: boolean }) {
  const m = useOnboarding()
  const [open, setOpen] = useState(false)
  if (!m.needed) return null
  const expanded = !compact || open
  const done = 4 - m.rows.filter((r) => r.state === 'missing' || r.state === 'out').length
  return (
    <section className={`onb-banner${compact ? ' is-compact' : ''}`} aria-label="環境準備">
      <header className="onb-head">
        <div className="onb-title">
          <strong>環境準備</strong>
          <span className="onb-sub">
            {m.next === 'herdr'
              ? 'herdr 還沒連上'
              : m.next === 'cli'
                ? '還沒有可用的 agent CLI：先安裝並登入一種'
                : m.next === 'project'
                  ? '已有可用的 CLI，接著新增 Project'
                  : '接著在 Project 裡新增 Bot'}
            {compact ? `（${done}/4 CLI 就緒）` : ''}
          </span>
        </div>
        {compact ? (
          <button type="button" className="btn" aria-expanded={open} onClick={() => setOpen(!open)}>
            {open ? '收合' : '展開'}
          </button>
        ) : null}
      </header>
      {expanded ? <OnboardingChecklist /> : null}
    </section>
  )
}
