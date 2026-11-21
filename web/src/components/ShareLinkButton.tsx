import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { rotateShare, setShareEnabled, shareErrorText, type ShareState } from '../api/share'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { asyncCopyAvailable, copyText } from '../lib/copyText'
import { shareIcon } from '../lib/shareProfile'
import { useShareRequests } from '../hooks/useShareRequests'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { useT, type TFn } from '../lib/i18n'
import './shareLinkButton.css'

const MENU_WIDTH = 360

function noUrlText(s: ShareState, t: TFn): string {
  return s.needs_rotate
    ? t('這條連結（結尾 {token}）是舊版開的，拿不回完整網址；從旁邊的選單「重產連結」一次就好', { token: s.token_hint ?? '…' })
    : t('拿不到分享連結：config.toml 的 [share] base_url 沒設')
}

function LinkIcon() {
  return (
    <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
      <path
        d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  )
}

export function ShareLinkButton({ botId }: { botId: string }) {
  const t = useT()
  const profile = useStore((s) => s.bots.find((b) => b.id === botId)?.share_profile ?? null)
  const restricted = profile !== null
  const enabled = useStore((s) => s.bots.find((b) => b.id === botId)?.share_enabled === true)
  const rev = useStore((s) => s.shareRev[botId] ?? 0)
  const notify = useStore((s) => s.notify)
  const [fetched, setFetched] = useState<{ rev: number; share: ShareState } | null>(null)
  const share = fetched && fetched.rev === rev ? fetched.share : null
  const [busy, setBusy] = useState(false)
  const [menu, setMenu] = useState(false)
  const [menuAt, setMenuAt] = useState<{ top: number; left: number } | null>(null)
  const [confirm, setConfirm] = useState<'rotate' | 'off' | null>(null)
  const wrapRef = useRef<HTMLSpanElement>(null)
  const menuRef = useRef<HTMLSpanElement>(null)
  const mainRef = useRef<HTMLButtonElement>(null)
  const { currentRev, readAt, beginPost, applyPost, isCurrentPost, isCurrentRead } = useShareRequests(
    botId,
    (state, revision) => setFetched({ rev: revision, share: state }),
  )

  useEffect(() => {
    if (!restricted) return
    void readAt(rev).promise.catch(() => {})
  }, [botId, restricted, rev, readAt])

  useEffect(() => {
    if (!menu) return
    const close = (e: MouseEvent) => {
      const target = e.target as Node
      if (!wrapRef.current?.contains(target) && !menuRef.current?.contains(target)) setMenu(false)
    }
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setMenu(false)
    document.addEventListener('mousedown', close)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('keydown', esc)
    }
  }, [menu])

  const menuKeys = useMenuKeys(menu, menuRef, mainRef, () => setMenu(false))

  if (!restricted) return null
  const selectOnly = !asyncCopyAvailable()

  const copy = (s: ShareState, done: string) => {
    if (!s.url) return notify('error', noUrlText(s, t))
    void copyText(s.url).then((ok) => notify(ok ? 'info' : 'error', ok ? done : t('複製失敗，請手動複製：{url}', { url: s.url ?? '' })))
  }

  const openMenu = () => {
    const r = mainRef.current?.getBoundingClientRect()
    if (r) {
      const width = Math.min(MENU_WIDTH, window.innerWidth - 16)
      const left = Math.min(Math.max(8, r.right - width), window.innerWidth - width - 8)
      setMenuAt({ top: r.bottom + 4, left })
    } else setMenuAt(null)
    setMenu(true)
  }

  const run = async (what: () => Promise<ShareState>, done: string, selectDone?: string) => {
    setBusy(true)
    const post = beginPost()
    try {
      let response: ShareState
      try {
        response = await what()
      } catch (e) {
        if (isCurrentPost(post)) notify('error', shareErrorText(e))
        return
      }

      let effective = response
      try {
        effective = await applyPost(post, response)
      } catch (e) {
        notify('error', shareErrorText(e))
        return
      }
      if (effective.enabled && selectOnly && selectDone) {
        openMenu()
        notify('info', selectDone)
      } else if (effective.enabled) copy(effective, done)
      else notify('info', response.enabled ? t('分享已關閉') : done)
    } finally {
      setBusy(false)
    }
  }

  const toggleMenu = () => (menu ? setMenu(false) : openMenu())

  const onMain = () => {
    if (enabled) return toggleMenu()
    void run(() => setShareEnabled(botId, true), t('已開啟分享並複製連結'), t('已開啟分享：連結在選單裡，按「複製連結」'))
  }

  const onCopyFromMenu = () => {
    setMenu(false)
    if (selectOnly) {
      if (share?.enabled && share.url) copy(share, t('已複製分享連結'))
      return
    }
    const requestRev = currentRev()
    const request = readAt(requestRev)
    void request.promise.then(
      (s) => {
        if (!isCurrentRead(request)) return
        if (!s.enabled) {
          useStore.setState((state) => ({
            bots: state.bots.map((b) => (b.id === botId ? { ...b, share_enabled: false } : b)),
          }))
          notify('info', t('分享已關閉'))
          return
        }
        copy(s, t('已複製分享連結'))
      },
      (e) => notify('error', shareErrorText(e)),
    )
  }

  const trustedPrefix = profile === 'trusted' ? t('信任分享：拿到連結的人可以透過它操作這台機器。') : ''
  const sharingTitle = share?.enabled && share.url
    ? t('分享中：{url}。點一下開選單（第一項複製連結）', { url: share.url })
    : t('分享中。點一下開選單（第一項複製連結）')
  const btnTitle = `${trustedPrefix}${enabled ? sharingTitle : t('開啟分享，連結會複製到剪貼簿')}`

  return (
    <span className="share-link" ref={wrapRef}>
      {}
      <button
        ref={mainRef}
        type="button"
        className={`icon-btn share-link-main${enabled ? ' on' : ''}${profile === 'trusted' ? ' trusted' : ''}`}
        disabled={busy}
        aria-label={enabled ? t('複製分享連結') : t('開啟分享並複製連結')}
        aria-haspopup={enabled ? 'menu' : undefined}
        aria-expanded={enabled ? menu : undefined}
        title={btnTitle}
        onClick={onMain}
      >
        <LinkIcon />
      </button>
      {menu
        ?
          createPortal(
        <span
          ref={menuRef}
          className="share-link-menu"
          role="menu"
          aria-label={t('分享連結')}
          onKeyDown={menuKeys}
          style={
            menuAt
              ? { position: 'fixed', top: menuAt.top, left: menuAt.left }
                : undefined
          }
        >
          {}
          {share?.enabled && share.url ? (
            <div className="share-link-url-text" aria-label={t('分享連結')}>
              {share.url}
            </div>
          ) : (
            <span className="share-link-url-note">{share ? noUrlText(share, t) : t('讀取連結中…')}</span>
          )}
          {selectOnly && !(share?.enabled && share.url) ? null : (
            <button type="button" role="menuitem" tabIndex={-1} className="share-link-copy" onClick={onCopyFromMenu}>
              {t('複製連結')}
            </button>
          )}
          <button
            type="button"
            role="menuitem"
            tabIndex={-1}
            onClick={() => {
              setMenu(false)
              setConfirm('rotate')
            }}
          >
            {t('重產連結')}
          </button>
          <button
            type="button"
            role="menuitem"
            tabIndex={-1}
            className="danger"
            onClick={() => {
              setMenu(false)
              setConfirm('off')
            }}
          >
            {t('關閉分享')}
          </button>
        </span>,
          document.body,
          )
        : null}
      {}
      <ConfirmDialog
        open={confirm === 'rotate'}
        title={t('重產分享連結？')}
        body={t('舊連結立刻失效，拿著舊連結的人會看到「連結已失效」。新連結會複製到剪貼簿，要重新傳給對方。')}
        confirmLabel={t('重產')}
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void run(() => rotateShare(botId), t('已重產並複製新連結，舊連結已失效'), t('已重產連結，舊連結已失效：新連結在選單裡，按「複製連結」'))
        }}
      />
      <ConfirmDialog
        open={confirm === 'off'}
        title={t('關閉分享？')}
        body={t('連結立刻失效；對話與檔案都還在，之後再開會產生一條新連結。')}
        confirmLabel={t('關閉分享')}
        danger
        onCancel={() => setConfirm(null)}
        onConfirm={() => {
          setConfirm(null)
          void run(() => setShareEnabled(botId, false), t('已關閉分享'))
        }}
      />
    </span>
  )
}

export function ShareMark({ botId }: { botId: string }) {
  const t = useT()
  const profile = useStore((s) => s.bots.find((x) => x.id === botId)?.share_profile ?? null)
  const mark = useStore((s) => {
    const b = s.bots.find((x) => x.id === botId)
    return !b?.share_profile ? null : b.share_enabled ? 'on' : 'off'
  })
  if (!mark) return null
  const trusted = profile === 'trusted'
  const label = mark === 'on'
    ? (trusted ? t('信任分享中') : t('分享中'))
    : (trusted ? t('信任分享 bot（未分享）') : t('分享用 bot（未分享）'))
  return (
    <span className={`share-mark ${mark}${trusted ? ' trusted' : ''}`} role="img" aria-label={label} title={label}>
      {shareIcon(profile)}
    </span>
  )
}
