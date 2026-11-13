import { useEffect, useRef, useState } from 'react'
import type { OutboxFile } from '../api'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useT } from '../lib/i18n'
import { fileSize, isPreviewableImage, remainingLabel, remainingNow, splitOutboxName } from '../lib/outboxList'
import { blobFor } from '../lib/outboxPreviewCache'
import { useOutboxFiles } from './useOutboxFiles'
import './phoneOutboxTray.css'

/** 收合狀態記在這台裝置（一個值，不分 bot）。 */
const OPEN_KEY = 'am.phoneOutboxTray.open'

function readOpen(): boolean {
  try {
    return localStorage.getItem(OPEN_KEY) === '1'
  } catch {
    return false
  }
}

function writeOpen(open: boolean) {
  try {
    localStorage.setItem(OPEN_KEY, open ? '1' : '0')
  } catch {
    // 私密視窗、封鎖站台資料：記不住就算了，預設收合。
  }
}

/**
 * 手機（≤640px）對話下方、輸入框上方的「bot 給的檔案」收合列（使用者 2026-11-03：「手機版放在那個位置不合理。
 * 應該放到下面，做一個收合開關，並且要可以預覽」）。取代 #929 放在 Git 彈窗最下面的那個入口。
 *
 * 沒有檔案整條不畫；預設收合，有新檔只在列上的數字標出來，不自動展開。清單與開新分頁預覽與桌面托盤共用（手機不給下載）
 * `useOutboxFiles`；縮圖與全螢幕預覽共用 `outboxPreviewCache` 的 blob 快取。
 */
export function PhoneOutboxTray() {
  const phone = useMediaQuery(PHONE_QUERY)
  // 桌面不掛：清單由右側欄的托盤讀，這裡不必多讀一份。
  return phone ? <PhoneOutboxTrayInner /> : null
}

function PhoneOutboxTrayInner() {
  const t = useT()
  const { botId, files, fetchedAt, now, loadedFor, busy, openPreview } = useOutboxFiles()
  const [open, setOpen] = useState(readOpen)
  /** 按住縮圖時放大的那張（使用者 2026-11-07：不開 modal，按住看大圖、放開就回來）。 */
  const [peek, setPeek] = useState<OutboxFile | null>(null)
  /** 看過幾個檔：之後多出來的在列上標「+N 新」。`null`＝這顆 bot 的清單還沒讀到；第一次讀到的那批不算新。 */
  const [seen, setSeen] = useState<{ bot: string | null; count: number | null }>({ bot: botId, count: null })
  if (seen.bot !== botId) {
    setSeen({ bot: botId, count: null })
  } else if (loadedFor === botId) {
    // 第一次讀到、展開著（進來的檔使用者看得到）、或檔案被清掉（數字往下走）：都把看過的數對齊目前清單。
    if (seen.count === null || (open && seen.count !== files.length) || seen.count > files.length) setSeen({ bot: botId, count: files.length })
  }

  if (!botId || files.length === 0) return null

  const fresh = open || seen.count === null ? 0 : files.length - seen.count
  const images = files.filter((f) => isPreviewableImage(f.name))
  const others = files.filter((f) => !isPreviewableImage(f.name))
  const toggle = () => {
    const next = !open
    setOpen(next)
    writeOpen(next)
  }

  return (
    <section className={`phone-outbox${open ? ' open' : ''}`} aria-label={t('這顆 bot 交給你的檔案')}>
      <button type="button" className="phone-outbox-bar" aria-expanded={open} onClick={toggle}>
        <span aria-hidden="true">📎</span>
        <span className="phone-outbox-title">{t('bot 給的檔案（{n}）', { n: files.length })}</span>
        {fresh > 0 ? (
          <span className="phone-outbox-new" title={t('收合後新進來的檔案')}>
            {t('+{n} 新', { n: fresh })}
          </span>
        ) : null}
        <span className="spacer" />
        <span className="phone-outbox-caret" aria-hidden="true">
          {open ? '▾' : '▸'}
        </span>
      </button>
      {open ? (
        <div className="phone-outbox-body">
          {images.length > 0 ? (
            <ul className={`phone-outbox-thumbs${peek ? ' peeking' : ''}`} role="list">
              {images.map((f) => (
                <li key={f.name}>
                  <button
                    type="button"
                    className={`phone-outbox-thumb${peek?.name === f.name ? ' peeking' : ''}`}
                    data-outbox-name={f.name}
                    aria-label={t('按住預覽 {name}', { name: f.name })}
                    onPointerDown={(e) => {
                      // 滑鼠也要在別處放開時收得掉：把指標抓在這顆鈕上（觸控本來就是隱式抓取）。
                      e.currentTarget.setPointerCapture?.(e.pointerId)
                      setPeek(f)
                    }}
                    onPointerMove={(e) => {
                      // 按住滑過其他縮圖就換成那張（使用者 2026-11-07：兩張以上，碰到哪張就換 focus）。
                      // 指標抓在起點那顆鈕上，所以用座標找手指底下的縮圖；大圖層不吃指標，找得到底下的鈕。
                      const hit = document.elementFromPoint(e.clientX, e.clientY)?.closest<HTMLElement>('[data-outbox-name]')
                      const name = hit?.dataset.outboxName
                      if (name && name !== peek?.name) {
                        const next = images.find((x) => x.name === name)
                        if (next) setPeek(next)
                      }
                    }}
                    onPointerUp={() => setPeek(null)}
                    onPointerCancel={() => setPeek(null)}
                    onLostPointerCapture={() => setPeek(null)}
                    onKeyDown={(e) => {
                      if (e.key === ' ' || e.key === 'Enter') setPeek(f)
                    }}
                    onKeyUp={() => setPeek(null)}
                    onBlur={() => setPeek(null)}
                    // 長按不要跳出系統的圖片選單。
                    onContextMenu={(e) => e.preventDefault()}
                  >
                    <OutboxThumb botId={botId} file={f} />
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
          {/* 圖檔只畫縮圖、不列檔名（使用者 2026-11-04：手機不需要 show 檔名）；清單只留點不出縮圖的其他檔。 */}
          {others.length > 0 ? (
          <ul className="phone-outbox-list" role="list">
            {others.map((f) => {
              const left = remainingNow(f, fetchedAt, now)
              const { dir: folder, base } = splitOutboxName(f.name)
              const isBusy = busy?.bot === botId && busy.name === f.name
              return (
                <li key={f.name} className="phone-outbox-item">
                  <button
                    type="button"
                    className="outbox-file"
                    disabled={isBusy}
                    // 手機不給下載（使用者 2026-11-03）：圖檔點名字開放大預覽，其他檔在新分頁打開。
                    title={t('預覽 {name}', { name: f.name })}
                    onClick={() => void openPreview(f.name)}
                  >
                    <span className="outbox-name">
                      {folder ? <span className="outbox-dir">{folder}</span> : null}
                      {base}
                    </span>
                    <span className={`outbox-meta${left !== null && left <= 600 ? ' soon' : ''}`}>
                      {fileSize(f.size)}
                      {left === null ? '' : ` · ${remainingLabel(left)}`}
                      {isBusy ? t(' · 開啟中…') : ''}
                    </span>
                  </button>
                </li>
              )
            })}
          </ul>
          ) : null}
        </div>
      ) : null}
      {peek && botId ? (
        <div className="phone-outbox-peek" aria-live="polite">
          <OutboxThumb botId={botId} file={peek} full />
        </div>
      ) : null}
    </section>
  )
}

/** 縮圖與全螢幕預覽都走 `blobFor`：同一張圖只抓一次（token 帶不進 `<img src>`）。 */
/** 失敗後自動重試的等待（毫秒）；用完還失敗才顯示 ✕，按 ✕ 可再試。 */
const RETRY_DELAYS = [1500, 4000]

function OutboxThumb({ botId, file, full = false }: { botId: string; file: OutboxFile; full?: boolean }) {
  const t = useT()
  const key = `${botId}/${file.name}/${file.modified}`
  const [src, setSrc] = useState<{ key: string; url: string | null }>({ key: '', url: null })
  /** 縮圖進可視範圍才開始抓（全螢幕大圖一定要抓）：幾十張圖不再一展開就全部發請求。 */
  const holder = useRef<HTMLSpanElement>(null)
  const [visible, setVisible] = useState(full || typeof IntersectionObserver === 'undefined')
  /** 失敗重試的次數；改它就重新拿租約。 */
  const [attempt, setAttempt] = useState(0)
  useEffect(() => {
    if (visible) return
    const el = holder.current
    if (!el) return
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        setVisible(true)
        io.disconnect()
      }
    }, { rootMargin: '120px' })
    io.observe(el)
    return () => io.disconnect()
  }, [visible])
  useEffect(() => {
    if (!visible) return
    let alive = true
    let retry: ReturnType<typeof setTimeout> | null = null
    // 持有到卸掉為止：還在畫面上的圖不會被快取擠出 revoke（#2074）。
    const lease = blobFor(botId, file.name, file.modified)
    lease.url.then(
      (url) => alive && setSrc({ key, url }),
      () => {
        if (!alive) return
        const wait = RETRY_DELAYS[attempt]
        if (wait === undefined) setSrc({ key, url: null })
        else retry = setTimeout(() => alive && setAttempt((n) => n + 1), wait)
      },
    )
    return () => {
      alive = false
      if (retry) clearTimeout(retry)
      lease.release()
    }
  }, [botId, file.name, file.modified, key, visible, attempt])
  const cls = full ? 'phone-outbox-full' : 'phone-outbox-thumb-img'
  if (src.key !== key) return <span ref={holder} className={`${cls} note`}>{full ? t('載入預覽…') : '…'}</span>
  if (!src.url)
    return (
      <span
        className={`${cls} note`}
        role="button"
        onClick={(e) => {
          e.stopPropagation()
          setSrc({ key: '', url: null })
          setAttempt(0)
        }}
      >
        {full ? t('預覽載不出來') : '✕'}
      </span>
    )
  return <img className={cls} src={src.url} alt={file.name} />
}
