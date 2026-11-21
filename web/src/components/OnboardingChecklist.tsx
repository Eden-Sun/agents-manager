import { useState } from 'react'
import { ADD_PROJECT_EVENT, KIND_STATE_LABEL, type OnbKindRow } from '../lib/onboarding'
import { useOnboarding } from '../lib/useOnboarding'
import { installBusyKey, installKindOnHost } from '../lib/onboardingActions'
import { AGY_LOGIN_GUIDE, CODEX_LOGIN_GUIDE, cliLoginCommand } from '../lib/quotaLogin'
import { useStore } from '../store/store'
import { AgyInstallButton } from './AgyInstall'
import { useT } from '../lib/i18n'
import { KindTag } from './KindTag'
import { KIND_LABEL } from './kindMeta'
import { QuotaLoginShell } from './QuotaLoginShell'
import './onboarding.css'

function KindRowView({ row }: { row: OnbKindRow }) {
  const t = useT()
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
        {t.tr(KIND_STATE_LABEL[row.state])}
        {row.state !== 'missing' && row.version ? ` ${row.version}` : ''}
        {row.account ? t('（{account}）', { account: row.account }) : ''}
      </span>
      {row.state === 'missing' ? (
        row.kind === 'agy' ? (
          <div className="onb-act">
            <AgyInstallButton host="local" />
          </div>
        ) : (
          <button
            type="button"
            className="btn primary onb-act"
            disabled={installBusy}
            title={t('在本機開 shell 執行 {kind} 的官方安裝指令', { kind: label })}
            onClick={() => void installKindOnHost('local', row.kind)}
          >
            {installBusy ? t('安裝中…') : t('安裝')}
          </button>
        )
      ) : row.state === 'out' ? (
        <div className="onb-act">
          <QuotaLoginShell host="local" hostLabel={t('本機')} kind={row.kind} command={cliLoginCommand(row.kind, {})} guide={row.kind === 'agy' ? t.tr(AGY_LOGIN_GUIDE) : row.kind === 'codex' ? t.tr(CODEX_LOGIN_GUIDE) : undefined} />
        </div>
      ) : null}
    </li>
  )
}

export function OnboardingChecklist() {
  const t = useT()
  const m = useOnboarding()
  const projects = useStore((s) => s.projects)
  const requestOpenBotSheet = useStore((s) => s.requestOpenBotSheet)
  const refreshTools = useStore((s) => s.refreshTools)
  const refreshState = useStore((s) => s.refreshState)
  const toolsBusy = useStore((s) => s.busy['tools:local'] === true)
  return (
    <div className="onb" role="group" aria-label={t('環境準備')}>
      <ul className="onb-list">
        <li className={`onb-row is-${m.herdrConnected ? 'ok' : 'missing'}`} data-kind="herdr" data-state={m.herdrConnected ? 'ok' : 'missing'}>
          <span className="onb-mark" aria-hidden="true">
            {m.herdrConnected ? '✓' : '✗'}
          </span>
          <span className="onb-name">herdr</span>
          <span className="onb-state">{m.herdrConnected ? t('已連線') : t('連不上——請確認 herdr 已安裝並在背景執行')}</span>
          {m.herdrConnected ? null : (
            <button type="button" className="btn onb-act" onClick={() => void refreshState()}>
              {t('重試')}
            </button>
          )}
        </li>
        {m.rows.map((r) => (
          <KindRowView key={r.kind} row={r} />
        ))}
      </ul>
      <div className="onb-foot">
        <button type="button" className="btn" disabled={toolsBusy} title={t('重新問本機的 CLI 現在裝了什麼、登入了誰')} onClick={() => void refreshTools('local')}>
          {toolsBusy ? t('偵測中…') : t('重新偵測')}
        </button>
        {m.needed ? (
          <ol className="onb-steps" aria-label={t('接下來')}>
            <li className={m.anyReady ? 'is-done' : 'is-now'}>{t('安裝並登入至少一種 CLI')}</li>
            <li className={projects.length > 0 ? 'is-done' : m.anyReady ? 'is-now' : ''}>
              <button type="button" className="btn" disabled={!m.anyReady} onClick={() => window.dispatchEvent(new Event(ADD_PROJECT_EVENT))}>
                {t('新增 Project')}
              </button>
            </li>
            <li className={projects.length > 0 && m.anyReady ? 'is-now' : ''}>
              <button type="button" className="btn primary" disabled={!m.anyReady || projects.length === 0} onClick={() => requestOpenBotSheet(projects[0].id)}>
                {t('新增 Bot')}
              </button>
            </li>
          </ol>
        ) : null}
      </div>
      {m.needed ? null : <p className="onb-all hint">{t('環境已就緒。')}</p>}
    </div>
  )
}

export function OnboardingBanner({ compact }: { compact: boolean }) {
  const t = useT()
  const m = useOnboarding()
  const [open, setOpen] = useState(false)
  if (!m.needed) return null
  const expanded = !compact || open
  const done = 4 - m.rows.filter((r) => r.state === 'missing' || r.state === 'out').length
  return (
    <section className={`onb-banner${compact ? ' is-compact' : ''}`} aria-label={t('環境準備')}>
      <header className="onb-head">
        <div className="onb-title">
          <strong>{t('環境準備')}</strong>
          <span className="onb-sub">
            {m.next === 'herdr'
              ? t('herdr 還沒連上')
              : m.next === 'cli'
                ? t('還沒有可用的 agent CLI：先安裝並登入一種')
                : m.next === 'project'
                  ? t('已有可用的 CLI，接著新增 Project')
                  : t('接著在 Project 裡新增 Bot')}
            {compact ? t('（{done}/4 CLI 就緒）', { done }) : ''}
          </span>
        </div>
        {compact ? (
          <button type="button" className="btn" aria-expanded={open} onClick={() => setOpen(!open)}>
            {open ? t('收合') : t('展開')}
          </button>
        ) : null}
      </header>
      {expanded ? <OnboardingChecklist /> : null}
    </section>
  )
}
