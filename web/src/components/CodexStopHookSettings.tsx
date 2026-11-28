import { useCallback, useEffect, useState } from 'react'
import { fetchStopHook, trustStopHook, type StopHookEntry, type StopHookReport, type StopHookStatus } from '../api/codexStopHook'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

export function CodexStopHookSettings() {
  const t = useT()
  const [report, setReport] = useState<StopHookReport | null>(null)
  const [pending, setPending] = useState<StopHookEntry | null>(null)
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)
  const [loadErr, setLoadErr] = useState<string | null>(null)

  const load = useCallback(() => {
    setLoadErr(null)
    fetchStopHook().then(
      (r) => (Array.isArray(r?.entries) ? setReport(r) : setLoadErr(t('回應格式不對'))),
      (e: unknown) => setLoadErr(e instanceof Error ? e.message : String(e)),
    )
  }, [t])

  useEffect(load, [load])

  const label = (e: StopHookEntry) => `${e.host} / ${e.identity ?? t('預設帳號')}`
  const statusText = (status: StopHookStatus | undefined) => {
    switch (status) {
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

  const needsTrust = (status: StopHookStatus | undefined) => status === 'not_installed' || status === 'untrusted' || status === 'hash_mismatch'

  const confirmTrust = async () => {
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

  const title = <h4 className="codex-stop-hook-title">{t('Codex Stop hook 信任')}</h4>
  if (!report) {
    return (
      <section className="codex-stop-hook-settings" aria-label={t('Codex Stop hook 信任')}>
        {title}
        {loadErr ? (
          <p className="codex-stop-hook-error" role="alert">
            {t('讀取 Stop hook 狀態失敗：{reason}', { reason: loadErr })}{' '}
            <button type="button" onClick={load}>{t('重試')}</button>
          </p>
        ) : (
          <p className="codex-stop-hook-note">{t('讀取中…')}</p>
        )}
      </section>
    )
  }
  return (
    <section className="codex-stop-hook-settings" aria-label={t('Codex Stop hook 信任')}>
      {title}
      <p className="codex-stop-hook-note">
        {report.enabled
          ? t('信任後，AG Man 會停用會爆 argv 上限的舊通知，改用 stdin Stop hook。')
          : t('尚未開啟（設定檔 [codex] stop_hook = false），目前不會安裝或使用。')}{' '}
        {t('同一次信任也會寫入廣域 kill 守門（PreToolUse）；未信任的帳號，codex bot 只剩 PATH 上的 pkill／killall shim，擋不到 kill -1、xargs kill。')}
      </p>
      <ul className="codex-stop-hook-list">
        {report.entries.map((e) => (
          <li key={`${e.host}/${e.identity ?? ''}`}>
            <span>{label(e)}</span> <span>{t('Stop：{status}', { status: statusText(e.status) })}</span>{' '}
            <span>{t('kill 守門：{status}', { status: statusText(e.kill_guard) })}</span>{' '}
            {e.kill_guard !== 'trusted' ? <strong className="codex-kill-guard-off">{t('codex kill 守門未生效')}</strong> : null}{' '}
            {needsTrust(e.status) || needsTrust(e.kill_guard) ? (
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
        body={pending ? t('這會讓 {target} 的 codex 在每個回合結束時執行 AG Man 的 stop hook、在每條 shell 指令前過廣域 kill 守門，並在該帳號目錄的 config.toml 寫入對應的 trusted_hash（寫入前會備份）。', { target: label(pending) }) : ''}
        confirmLabel={t('信任此 hook')}
        onConfirm={() => void confirmTrust()}
        onCancel={() => setPending(null)}
      />
    </section>
  )
}
