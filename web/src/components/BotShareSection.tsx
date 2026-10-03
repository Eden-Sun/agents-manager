import { useEffect, useState } from 'react'
import { fetchShare, rotateShare, setShareEnabled, shareErrorText, type ShareState } from '../api/share'
import { copyText } from '../lib/copyText'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import './botShare.css'

const fmt = (iso: string | null) => (iso ? new Date(iso).toLocaleString('zh-TW', { hour12: false }) : '—')

/**
 * Bot 設定裡的「分享」區塊（SPEC「分享 bot」）：只有分享用（受限）的 bot 才畫。
 * 開／關、複製連結、重產（舊的立刻失效）。完整連結只在開或重產的那一刻拿得到，之後只剩末 4 碼。
 */
export function BotShareSection({ botId }: { botId: string }) {
  const restricted = useStore((s) => s.bots.find((b) => b.id === botId)?.share_profile === 'restricted')
  const notify = useStore((s) => s.notify)
  const [share, setShare] = useState<ShareState | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState<'rotate' | 'off' | null>(null)

  useEffect(() => {
    if (!restricted) return
    let alive = true
    fetchShare(botId).then(
      (s) => alive && setShare(s),
      (e) => alive && setError(shareErrorText(e)),
    )
    return () => {
      alive = false
    }
  }, [botId, restricted])

  if (!restricted) return null

  const run = async (what: () => Promise<ShareState>, done?: string) => {
    setBusy(true)
    setError(null)
    try {
      // 新回應沒帶 url（關掉）就清掉手上那條，不然畫面上還留著一條已失效的連結。
      setShare(await what())
      if (done) notify('info', done)
    } catch (e) {
      setError(shareErrorText(e))
    } finally {
      setBusy(false)
    }
  }

  const copy = (url: string) => void copyText(url).then((ok) => notify(ok ? 'info' : 'error', ok ? '已複製分享連結' : '複製失敗，請手動選取連結'))
  const enabled = share?.enabled ?? false

  return (
    <section className="bs-share" aria-label="分享">
      <div className="bs-share-head">
        <strong>🔗 分享給外部使用者</strong>
        <label className="bs-share-toggle">
          <input
            type="checkbox"
            role="switch"
            checked={enabled}
            disabled={busy || share === null}
            onChange={(e) => (e.target.checked ? void run(() => setShareEnabled(botId, true), '已開啟分享') : setConfirm('off'))}
          />
          {enabled ? '分享中' : '未分享'}
        </label>
      </div>
      <p className="hint">
        拿到連結的人＝網路上任何人：可以跟這顆 bot 對話、上傳檔案、下載它給的檔案，看得到完整對話歷史；看不到 AG Man 的其他東西。
      </p>
      {error ? (
        <p className="bs-share-error" role="alert">
          {error}
        </p>
      ) : null}
      {enabled ? (
        <div className="bs-share-body">
          {share?.url ? (
            <>
              <div className="bs-share-url">
                <input type="text" readOnly value={share.url} aria-label="分享連結" onFocus={(e) => e.currentTarget.select()} />
                <button type="button" className="btn primary" onClick={() => copy(share.url!)}>
                  複製連結
                </button>
              </div>
              <p className="hint">完整連結只顯示這一次（daemon 只存雜湊）；關掉設定後就只看得到末四碼，忘了就重產。</p>
            </>
          ) : (
            <p className="bs-share-hint">
              目前的連結結尾 <code>{share?.token_hint ?? '…'}</code>。完整連結只在產生時顯示，要再拿一次請「重產連結」。
            </p>
          )}
          <dl className="bs-share-meta">
            <dt>建立於</dt>
            <dd>{fmt(share?.created_at ?? null)}</dd>
            <dt>最後使用</dt>
            <dd>{fmt(share?.last_used_at ?? null)}</dd>
          </dl>
          <div className="bs-share-actions">
            <button type="button" className="btn" disabled={busy} onClick={() => setConfirm('rotate')}>
              重產連結
            </button>
            <button type="button" className="btn danger" disabled={busy} onClick={() => setConfirm('off')}>
              關閉分享
            </button>
          </div>
        </div>
      ) : null}
      {/* 兩個框分開：同一個框有 800ms 防連點，重產完馬上關閉會被吃掉。 */}
      <ConfirmDialog
        open={confirm === 'rotate'}
        title="重產分享連結？"
        body="舊連結立刻失效，拿著舊連結的人會看到「連結已失效」。新連結要重新傳給對方。"
        confirmLabel="重產"
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void run(() => rotateShare(botId), '已重產連結，舊連結已失效')
        }}
      />
      <ConfirmDialog
        open={confirm === 'off'}
        title="關閉分享？"
        body="連結立刻失效；對話與檔案都還在，之後再開會產生一條新連結。"
        confirmLabel="關閉分享"
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void run(() => setShareEnabled(botId, false), '已關閉分享')
        }}
      />
    </section>
  )
}
