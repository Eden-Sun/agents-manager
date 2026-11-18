import { useEffect, useState } from 'react'
import { fetchStopHook, trustStopHook, type StopHookEntry, type StopHookReport } from '../api/codexStopHook'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

/** #1989：顯示各主機 × codex 帳號目錄的 Stop hook 信任狀態；使用者確認後才寫入 trusted_hash。 */
export function CodexStopHookSettings() {
  const t = useT()
  const [report, setReport] = useState<StopHookReport | null>(null)
  const [pending, setPending] = useState<StopHookEntry | null>(null)
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)

  useEffect(() => {
    fetchStopHook().then((r) => setReport(Array.isArray(r?.entries) ? r : null), () => {})
  }, [])

  const label = (e: StopHookEntry) => `${e.host} / ${e.identity ?? t('預設帳號')}`
  const statusText = (e: StopHookEntry) => {
    switch (e.status) {
      case 'trusted':
        return t('已信任')
      case 'untrusted':
        return t('未信任')
      case 'hash_mismatch':
        return t('雜湊不符（hook 被改過）')
      case 'not_installed':
        return t('尚未安裝')
      default:
        return t('無法讀取')
    }
  }

  const confirm = async () => {
    if (!pending) return
    const target = pending
    setBusy(true)
    setMsg(null)
    try {
      const updated = await trustStopHook(target.host, target.identity)
      setReport((prev) => prev && { ...prev, entries: prev.entries.map((x) => (x.host === target.host && x.identity === target.identity ? updated : x)) })
    } catch (e) {
      setMsg(e instanceof Error ? e.message : t('操作失敗'))
    } finally {
      setBusy(false)
      setPending(null)
    }
  }

  if (!report) return null
  return (
    <section className="codex-stop-hook-settings" aria-label={t('Codex Stop hook 信任')}>
      <h4 className="codex-stop-hook-title">{t('Codex Stop hook 信任')}</h4>
      <p className="codex-stop-hook-note">
        {report.enabled
          ? t('信任後，AG Man 會停用會爆 argv 上限的舊通知，改用 stdin Stop hook。')
          : t('尚未開啟（設定檔 [codex] stop_hook = false），目前不會安裝或使用。')}
      </p>
      <ul className="codex-stop-hook-list">
        {report.entries.map((e) => (
          <li key={`${e.host}/${e.identity ?? ''}`}>
            <span>{label(e)}</span> <span>{statusText(e)}</span>{' '}
            {e.status === 'not_installed' || e.status === 'untrusted' || e.status === 'hash_mismatch' ? (
              <button type="button" disabled={busy} onClick={() => setPending(e)}>
                {t('信任此 hook')}
              </button>
            ) : null}
          </li>
        ))}
      </ul>
      {msg ? <p className="codex-stop-hook-error" role="alert">{msg}</p> : null}
      <ConfirmDialog
        open={pending !== null}
        title={t('信任 AG Man 的 Codex Stop hook？')}
        body={pending ? t('這會讓 {target} 的 codex 在每個回合結束時執行 AG Man 的 stop hook，並在該帳號目錄的 config.toml 寫入一筆 trusted_hash（寫入前會備份）。', { target: label(pending) }) : ''}
        confirmLabel={t('信任此 hook')}
        onConfirm={() => void confirm()}
        onCancel={() => setPending(null)}
      />
    </section>
  )
}
