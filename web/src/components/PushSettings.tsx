import { useCallback, useEffect, useState } from 'react'
import { getPushSettings, listPushEndpoints, sendTestPush, setPushSettings, type PushSettingsValue } from '../api/push'
import { useT } from '../lib/i18n'
import { currentSubscription, disablePush, enablePush, pushSupport, type PushSupport } from '../lib/pushNotify'

type View = { kind: 'loading' } | { kind: 'unsupported'; reason: Exclude<PushSupport, { ok: true }>['reason'] } | { kind: 'denied' } | { kind: 'off' } | { kind: 'on' }

/** 環境設定→顯示：網頁推播（排程完成通知，手機鎖屏也收得到）。狀態＝這個瀏覽器有訂閱且 daemon 也記著。 */
export function PushSettings() {
  const t = useT()
  const [view, setView] = useState<View>({ kind: 'loading' })
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)
  const [opts, setOpts] = useState<PushSettingsValue>({ main_bots: true, other_bots: true, updates: true, reset_credits: true, auth: true, quota: true })
  useEffect(() => {
    getPushSettings().then((s) => setOpts((prev) => ({ ...prev, ...s })), () => {})
  }, [])
  const toggle = (key: keyof PushSettingsValue, v: boolean) => {
    setOpts((o) => ({ ...o, [key]: v }))
    setPushSettings({ [key]: v }).catch(() => setOpts((o) => ({ ...o, [key]: !v })))
  }

  const refresh = useCallback(async () => {
    const support = pushSupport()
    if (!support.ok) return setView({ kind: 'unsupported', reason: support.reason })
    if (Notification.permission === 'denied') return setView({ kind: 'denied' })
    try {
      const sub = await currentSubscription()
      const known = sub ? (await listPushEndpoints()).includes(sub.endpoint) : false
      setView({ kind: known ? 'on' : 'off' })
    } catch {
      setView({ kind: 'off' })
    }
  }, [])
  useEffect(() => {
    void refresh()
  }, [refresh])

  const run = async (fn: () => Promise<void>) => {
    setBusy(true)
    setMsg(null)
    try {
      await fn()
    } catch (e) {
      setMsg(e instanceof Error ? e.message : t('操作失敗'))
    } finally {
      setBusy(false)
      void refresh()
    }
  }

  const why = (reason: Exclude<PushSupport, { ok: true }>['reason']) =>
    reason === 'ios-home-screen'
      ? t('iPhone 要先「加入主畫面」，再從主畫面的圖示開啟，才能啟用推播。')
      : reason === 'insecure'
        ? t('需要 HTTPS 連線（安全環境）才能使用推播。')
        : t('這個瀏覽器不支援網頁推播。')

  return (
    <div className="push-settings">
      <div className="push-settings-title">{t('推播通知')}</div>
      <p className="push-settings-note" role="status">
        {view.kind === 'loading'
          ? t('檢查中…')
          : view.kind === 'unsupported'
            ? why(view.reason)
            : view.kind === 'denied'
              ? t('通知權限被封鎖：請到瀏覽器／系統設定允許這個網站的通知。')
              : view.kind === 'on'
                ? t('已啟用：排程完成、失敗或結果不明時，這台裝置會收到通知。')
                : t('未啟用：啟用後，排程完成、失敗或結果不明時這台裝置會收到通知（鎖屏也會）。')}
      </p>
      <div className="push-settings-actions">
        {view.kind === 'off' ? (
          <button type="button" className="btn" disabled={busy} onClick={() => void run(async () => { if ((await enablePush()) === 'denied') setMsg(t('沒有取得通知權限')) })}>
            {t('啟用推播')}
          </button>
        ) : null}
        {view.kind === 'on' ? (
          <>
            <button type="button" className="btn" disabled={busy} onClick={() => void run(async () => { const r = await sendTestPush(); setMsg(r.sent > 0 ? t('已送出測試通知') : r.failed > 0 ? t('沒有送出：推送服務拒絕') + (r.reasons?.length ? `（${r.reasons.join('；')}）` : '') : t('沒有送出（沒有可用的訂閱）')) })}>
              {t('送測試通知')}
            </button>
            <button type="button" className="btn" disabled={busy} onClick={() => void run(disablePush)}>
              {t('停用推播')}
            </button>
          </>
        ) : null}
      </div>
      {([
        ['main_bots', t('主力 bot 回覆完成／需要回應時推播')],
        ['other_bots', t('其他 bot 需要回應時推播')],
        ['updates', t('偵測到更新（AG Man／CLI）時推播')],
        ['reset_credits', t('codex 額度用完且有重置券時推播（只通知，不自動使用）')],
        ['auth', t('被登出／憑證失效時推播')],
        ['quota', t('撞額度上限時推播')],
      ] as const).map(([key, label]) => (
        <label key={key} className="push-settings-note" style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer' }}>
          <input type="checkbox" checked={opts[key] ?? true} onChange={(e) => toggle(key, e.target.checked)} /> {label}
        </label>
      ))}
      {msg ? <p className="push-settings-note" role="alert">{msg}</p> : null}
    </div>
  )
}
