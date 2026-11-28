import { useCallback, useEffect, useState } from 'react'
import {
  fetchAutoUpdateSettings,
  updateAutoUpdateSettings,
  type AutoUpdateSettings as SettingsData,
} from '../api/autoUpdateSettings'
import { useT } from '../lib/i18n'

const KINDS = ['claude', 'codex', 'agy', 'grok'] as const
type Kind = (typeof KINDS)[number]

export function AutoUpdateSettings() {
  const t = useT()
  const [settings, setSettings] = useState<SettingsData>({
    claude: false,
    codex: false,
    agy: false,
    grok: false,
  })
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)
  const [loaded, setLoaded] = useState(false)
  const [loadErr, setLoadErr] = useState<string | null>(null)

  const load = useCallback(() => {
    setLoadErr(null)
    fetchAutoUpdateSettings().then(
      (data) => {
        setSettings(data)
        setLoaded(true)
      },
      (e: unknown) => setLoadErr(e instanceof Error ? e.message : String(e)),
    )
  }, [])

  useEffect(load, [load])

  const toggle = async (kind: Kind, value: boolean) => {
    setSettings((prev) => ({ ...prev, [kind]: value }))
    setBusy(true)
    setMsg(null)
    try {
      const updated = await updateAutoUpdateSettings({ [kind]: value })
      setSettings(updated)
    } catch (e) {
      setSettings((prev) => ({ ...prev, [kind]: !value }))
      setMsg(e instanceof Error ? e.message : t('操作失敗'))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="auto-update-settings" aria-label={t('自動更新（不通知）')}>
      <h4 className="auto-update-settings-title">{t('自動更新（不通知）')}</h4>
      <p className="auto-update-settings-note">
        {t('有新版自動安裝，不跳通知；跑著的 bot 重啟後才換版（codex 會重啟閒置的 codex bot）。失敗時仍會通知。')}
      </p>
      <div className="auto-update-settings-switches" style={{ display: 'flex', gap: '16px', flexWrap: 'wrap', margin: '8px 0' }}>
        {KINDS.map((k) => (
          <label key={k} style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer' }}>
            <input
              type="checkbox"
              aria-label={t('{kind} 自動更新', { kind: k })}
              checked={settings[k]}
              disabled={busy || !loaded}
              onChange={(e) => void toggle(k, e.target.checked)}
            />
            <span>{k}</span>
          </label>
        ))}
      </div>
      {loadErr ? (
        <p className="auto-update-settings-error" role="alert" style={{ color: 'var(--danger)' }}>
          {t('讀取自動更新設定失敗：{reason}', { reason: loadErr })}{' '}
          <button type="button" onClick={load}>{t('重試')}</button>
        </p>
      ) : null}
      {msg ? <p className="auto-update-settings-error" role="alert" style={{ color: 'var(--danger)' }}>{msg}</p> : null}
    </section>
  )
}
