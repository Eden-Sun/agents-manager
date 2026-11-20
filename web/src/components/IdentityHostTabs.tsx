import { useT } from '../lib/i18n'

export interface HostTabItem {
  name: string
  label: string
  connected: boolean
  /** 已登入的帳號數（各 kind 的預設帳號＋config／shell 身份）；斷線時不顯示。 */
  loggedIn: number
  /** 異常：主機斷線，或 daemon 標了「要重新登入」。 */
  anomaly: boolean
}

const PANEL_ID = 'identity-host-panel'

/** 身份頁的主機分頁：每台主機一個 tab；斷線的 tab 仍在，標「斷線」。 */
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
