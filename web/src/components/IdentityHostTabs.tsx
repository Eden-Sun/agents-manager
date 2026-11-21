import { useT } from '../lib/i18n'

export interface HostTabItem {
  name: string
  label: string
  connected: boolean
  loggedIn: number
  anomaly: boolean
}

const PANEL_ID = 'identity-host-panel'

export function IdentityHostTabs({ tabs, value, onChange }: { tabs: HostTabItem[]; value: string; onChange: (name: string) => void }) {
  const t = useT()
  return (
    <div className="tabs small identity-host-tabs" role="tablist" aria-label={t('主機')}>
      {tabs.map((tab) => {
        const selected = tab.name === value
        return (
          <button
            key={tab.name}
            type="button"
            className="tab"
            role="tab"
            id={`identity-tab-${tab.name}`}
            aria-selected={selected}
            aria-controls={selected ? PANEL_ID : undefined}
            onClick={() => onChange(tab.name)}
          >
            {tab.label}
            {tab.connected ? <span className="identity-tab-count">{t('{n} 已登入', { n: tab.loggedIn })}</span> : null}
            {!tab.connected ? <span className="identity-tab-off">{t('斷線')}</span> : null}
            {tab.anomaly ? <span className="identity-tab-dot" role="img" aria-label={t('有異常')} /> : null}
          </button>
        )
      })}
    </div>
  )
}

export { PANEL_ID as IDENTITY_HOST_PANEL_ID }
