import { useEffect, useRef, useState } from 'react'
import { fetchShare, rotateShare, setShareEnabled, shareErrorText, type ShareState } from '../api/share'
import { copyText } from '../lib/copyText'
import { shareIcon } from '../lib/shareProfile'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import './shareLinkButton.css'

/** 分享連結拿不到完整網址時的人話（舊資料只有雜湊，或 base_url 沒設）。 */
function noUrlText(s: ShareState): string {
  return s.needs_rotate
    ? `這條連結（結尾 ${s.token_hint ?? '…'}）是舊版開的，拿不回完整網址；從旁邊的選單「重產連結」一次就好`
    : '拿不到分享連結：config.toml 的 [share] base_url 沒設'
}

/**
 * 分享用 bot 的聊天區頂端「🔗 分享」（SPEC「分享 bot」）：一般 bot 不畫。信任分享（trusted）是 🔓、另一個顏色，一眼分得出來。
 * 未分享時按下＝開啟並複製連結；分享中按下＝複製連結。旁邊的 ▾ 選單可重產／關閉（都要確認）。
 */
export function ShareLinkButton({ botId }: { botId: string }) {
  const profile = useStore((s) => s.bots.find((b) => b.id === botId)?.share_profile ?? null)
  const restricted = profile !== null
  const enabled = useStore((s) => s.bots.find((b) => b.id === botId)?.share_enabled === true)
  // 重產連結不改 `share_enabled`：靠 store 的 `shareRev`（每收到一次 `bot_share_changed` 加一）知道連結換了（#1098）。
  const rev = useStore((s) => s.shareRev[botId] ?? 0)
  const notify = useStore((s) => s.notify)
  // 連結連同它是哪一版（`rev`）一起記：別處重產之後（rev 變了），舊的那條就不認，等重抓回來（#1098）。
  const [fetched, setFetched] = useState<{ rev: number; share: ShareState } | null>(null)
  const share = fetched && fetched.rev === rev ? fetched.share : null
  const [busy, setBusy] = useState(false)
  const [menu, setMenu] = useState(false)
  // 選單用 fixed 定位貼著 ▾：名字列 overflow 會剪掉 absolute 的選單（#1072）。
  const [menuAt, setMenuAt] = useState<{ top: number; right: number } | null>(null)
  const [confirm, setConfirm] = useState<'rotate' | 'off' | null>(null)
  const wrapRef = useRef<HTMLSpanElement>(null)
  const moreRef = useRef<HTMLButtonElement>(null)

  // 分享中就先把連結抓好：按下去同步複製，不用等網路（剪貼簿要在使用者手勢裡）。
  useEffect(() => {
    if (!restricted || !enabled) return
    let alive = true
    fetchShare(botId).then(
      (s) => alive && setFetched({ rev, share: s }),
      () => {},
    )
    return () => {
      alive = false
    }
  }, [botId, restricted, enabled, rev])

  useEffect(() => {
    if (!menu) return
    const close = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setMenu(false)
    }
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setMenu(false)
    document.addEventListener('mousedown', close)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('keydown', esc)
    }
  }, [menu])

  if (!restricted) return null

  const copy = (s: ShareState, done: string) => {
    if (!s.url) return notify('error', noUrlText(s))
    void copyText(s.url).then((ok) => notify(ok ? 'info' : 'error', ok ? done : `複製失敗，請手動複製：${s.url}`))
  }

  const run = async (what: () => Promise<ShareState>, done: string) => {
    setBusy(true)
    try {
      const s = await what()
      setFetched({ rev, share: s })
      if (s.enabled) copy(s, done)
      else notify('info', done)
    } catch (e) {
      notify('error', shareErrorText(e))
    } finally {
      setBusy(false)
    }
  }

  const toggleMenu = () => {
    if (!menu) {
      const r = moreRef.current?.getBoundingClientRect()
      setMenuAt(r ? { top: r.bottom + 4, right: Math.max(0, window.innerWidth - r.right) } : null)
    }
    setMenu((m) => !m)
  }

  const onMain = () => {
    // 手上那份要是開著的那條才直接複製；store 說關了（別處關掉的）就走「開啟」。
    if (enabled && share?.enabled && share.url) return copy(share, '已複製分享連結')
    void run(() => setShareEnabled(botId, true), enabled ? '已複製分享連結' : '已開啟分享並複製連結')
  }

  return (
    <span className="share-link" ref={wrapRef}>
      {/* 只放圖示、跟 ★ ⚙ 同一組（#1072）；文字「複製連結／分享」在 aria-label 與 title。 */}
      <button
        type="button"
        className={`icon-btn share-link-main${enabled ? ' on' : ''}${profile === 'trusted' ? ' trusted' : ''}`}
        disabled={busy}
        aria-label={enabled ? '複製分享連結' : '開啟分享並複製連結'}
        title={`${profile === 'trusted' ? '信任分享：拿到連結的人可以透過它操作這台機器。' : ''}${enabled ? `分享中${share?.enabled && share.url ? `：${share.url}` : ''}。點一下複製連結` : '開啟分享，連結會複製到剪貼簿'}`}
        onClick={onMain}
      >
        {shareIcon(profile)}
      </button>
      {enabled ? (
        <button
          ref={moreRef}
          type="button"
          className={`btn share-link-more${enabled ? ' on' : ''}`}
          aria-label="分享選項"
          aria-haspopup="menu"
          aria-expanded={menu}
          disabled={busy}
          onClick={toggleMenu}
        >
          ▾
        </button>
      ) : null}
      {menu ? (
        <span className="share-link-menu" role="menu" style={menuAt ? { position: 'fixed', top: menuAt.top, right: menuAt.right } : undefined}>
          <button
            type="button"
            role="menuitem"
            onClick={() => {
              setMenu(false)
              setConfirm('rotate')
            }}
          >
            重產連結
          </button>
          <button
            type="button"
            role="menuitem"
            className="danger"
            onClick={() => {
              setMenu(false)
              setConfirm('off')
            }}
          >
            關閉分享
          </button>
        </span>
      ) : null}
      {/* 兩個框分開：同一個框有 800ms 防連點（同 BotShareSection）。 */}
      <ConfirmDialog
        open={confirm === 'rotate'}
        title="重產分享連結？"
        body="舊連結立刻失效，拿著舊連結的人會看到「連結已失效」。新連結會複製到剪貼簿，要重新傳給對方。"
        confirmLabel="重產"
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void run(() => rotateShare(botId), '已重產並複製新連結，舊連結已失效')
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
    </span>
  )
}

/** 側欄 bot 名字旁的標記：受限 🔗、信任分享 🔓（另一個顏色）；分享中亮起。 */
export function ShareMark({ botId }: { botId: string }) {
  const profile = useStore((s) => s.bots.find((x) => x.id === botId)?.share_profile ?? null)
  const mark = useStore((s) => {
    const b = s.bots.find((x) => x.id === botId)
    return !b?.share_profile ? null : b.share_enabled ? 'on' : 'off'
  })
  if (!mark) return null
  const trusted = profile === 'trusted'
  const label = mark === 'on' ? (trusted ? '信任分享中' : '分享中') : trusted ? '信任分享 bot（未分享）' : '分享用 bot（未分享）'
  return (
    <span className={`share-mark ${mark}${trusted ? ' trusted' : ''}`} role="img" aria-label={label} title={label}>
      {shareIcon(profile)}
    </span>
  )
}
