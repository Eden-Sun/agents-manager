import { useEffect, useState } from 'react'
import { fetchShare, rotateShare, setShareEnabled, shareErrorText, type ShareState } from '../api/share'
import { copyText } from '../lib/copyText'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import './botShare.css'

const fmt = (iso: string | null) => (iso ? new Date(iso).toLocaleString('zh-TW', { hour12: false }) : '—')

/**
 * Bot 設定裡的「分享」區塊（SPEC「分享 bot」）：只有分享用的 bot（受限或信任分享）才畫。
 * 開／關、隨時顯示完整連結＋複製、重產（舊的立刻失效）。舊版開的分享只有雜湊，提示重產一次。
 * 聊天區頂端的「🔗 分享」或別處的重產會發 `bot_share_changed`：開／關會讓 `share_enabled` 變，重產不會（前後都是分享中），
 * 所以另外看 `shareRev`（每收到一次加一）——兩者任一變了就重抓（#1099）。
 */
export function BotShareSection({ botId }: { botId: string }) {
  const profile = useStore((s) => s.bots.find((b) => b.id === botId)?.share_profile ?? null)
  const restricted = profile !== null
  const sharing = useStore((s) => s.bots.find((b) => b.id === botId)?.share_enabled === true)
  const rev = useStore((s) => s.shareRev[botId] ?? 0)
  const notify = useStore((s) => s.notify)
  const online = useStore((s) => s.socket === 'open')
  const [share, setShare] = useState<ShareState | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState<'rotate' | 'off' | null>(null)
  const [attempt, setAttempt] = useState(0)

  /** 一次都沒讀到：開關是鎖著的，連回來要自己再抓。 */
  const loadFailed = share === null && error !== null
  useEffect(() => {
    if (online && loadFailed) setAttempt((n) => n + 1)
  }, [online, loadFailed])

  useEffect(() => {
    if (!restricted) return
    let alive = true
    fetchShare(botId).then(
      (s) => {
        if (!alive) return
        setShare(s)
        setError(null)
      },
      (e) => alive && setError(shareErrorText(e)),
    )
    return () => {
      alive = false
    }
  }, [botId, restricted, sharing, rev, attempt])

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
        <strong>{profile === 'trusted' ? '🔓 信任分享給外部使用者' : '🔗 分享給外部使用者'}</strong>
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
      {profile === 'trusted' ? (
        <p className="bs-share-trusted" role="note">
          拿到連結的人可以透過這顆 bot 操作這台機器上的任何東西（跑指令、改檔、git…）。只分享給絕對信任的人。
        </p>
      ) : null}
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
          {profile === 'trusted' ? (
            <div className="bs-share-embed">
              <label>
                <input
                  type="checkbox"
                  checked={share?.allow_embed ?? false}
                  disabled={busy || share === null}
                  onChange={(e) => void run(() => setShareEnabled(botId, true, e.target.checked), e.target.checked ? '已允許 iframe 嵌入' : '已關閉 iframe 嵌入')}
                />{' '}
                允許 iframe 嵌入
              </label>
              <p className="hint">任何網站都能把這個分享頁嵌進去；只在你要嵌到自己的網站時開。</p>
            </div>
          ) : null}
          {share?.url ? (
            <>
              <div className="bs-share-url">
                <input type="text" readOnly value={share.url} aria-label="分享連結" onFocus={(e) => e.currentTarget.select()} />
                <button type="button" className="btn primary" onClick={() => copy(share.url!)}>
                  複製連結
                </button>
              </div>
              <p className="hint">聊天區頂端的「🔗 複製連結」也能隨時複製。</p>
            </>
          ) : share?.needs_rotate ? (
            <p className="bs-share-hint">
              目前的連結結尾 <code>{share.token_hint ?? '…'}</code>，是舊版開的（只存雜湊），拿不回完整網址；「重產連結」一次之後就會一直顯示在這裡。
            </p>
          ) : (
            <p className="bs-share-hint">
              目前的連結結尾 <code>{share?.token_hint ?? '…'}</code>。config.toml 的 [share] base_url 沒設，組不出完整網址。
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
