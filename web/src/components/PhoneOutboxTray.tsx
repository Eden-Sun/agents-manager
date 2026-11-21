import { useEffect, useRef, useState } from 'react'
import type { OutboxFile } from '../api'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useT } from '../lib/i18n'
import { fileSize, isPreviewableImage, remainingLabel, remainingNow, splitOutboxName } from '../lib/outboxList'
import { blobFor } from '../lib/outboxPreviewCache'
import { useOutboxFiles } from './useOutboxFiles'
import './phoneOutboxTray.css'

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
  }
}

export function PhoneOutboxTray() {
  const phone = useMediaQuery(PHONE_QUERY)
  return phone ? <PhoneOutboxTrayInner /> : null
}

function PhoneOutboxTrayInner() {
  const t = useT()
  const { botId, files, fetchedAt, now, loadedFor, busy, openPreview } = useOutboxFiles()
  const [open, setOpen] = useState(readOpen)
  const [peek, setPeek] = useState<OutboxFile | null>(null)
  const [seen, setSeen] = useState<{ bot: string | null; count: number | null }>({ bot: botId, count: null })
  if (seen.bot !== botId) {
    setSeen({ bot: botId, count: null })
  } else if (loadedFor === botId) {
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
                <li key={`${f.owner ?? ''}/${f.name}`}>
                  <button
                    type="button"
                    className={`phone-outbox-thumb${peek?.name === f.name ? ' peeking' : ''}`}
                    data-outbox-name={f.name}
                    aria-label={t('按住預覽 {name}', { name: f.name })}
                    onPointerDown={(e) => {
                      e.currentTarget.setPointerCapture?.(e.pointerId)
                      setPeek(f)
                    }}
                    onPointerMove={(e) => {
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
                    onContextMenu={(e) => e.preventDefault()}
                  >
                    <OutboxThumb botId={f.owner ?? botId} file={f} />
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
          {}
          {others.length > 0 ? (
          <ul className="phone-outbox-list" role="list">
            {others.map((f) => {
              const left = remainingNow(f, fetchedAt, now)
              const { dir: folder, base } = splitOutboxName(f.name)
              const isBusy = busy?.bot === (f.owner ?? botId) && busy.name === f.name
              return (
                <li key={`${f.owner ?? ''}/${f.name}`} className="phone-outbox-item">
                  <button
                    type="button"
                    className="outbox-file"
                    disabled={isBusy}
                    title={t('預覽 {name}', { name: f.name })}
                    onClick={() => void openPreview(f.name, f.owner)}
                  >
                    <span className="outbox-name">
                      {folder ? <span className="outbox-dir">{folder}</span> : null}
                      {base}
                    </span>
                    <span className={`outbox-meta${left !== null && left <= 600 ? ' soon' : ''}`}>
                      {fileSize(f.size)}
                      {f.ownerName ? t(' · 來自 {name}', { name: f.ownerName }) : ''}
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
          <OutboxThumb botId={peek.owner ?? botId} file={peek} full />
        </div>
      ) : null}
    </section>
  )
}

const RETRY_DELAYS = [1500, 4000]

function OutboxThumb({ botId, file, full = false }: { botId: string; file: OutboxFile; full?: boolean }) {
  const t = useT()
  const key = `${botId}/${file.name}/${file.modified}`
  const [src, setSrc] = useState<{ key: string; url: string | null }>({ key: '', url: null })
  const holder = useRef<HTMLSpanElement>(null)
  const [visible, setVisible] = useState(full || typeof IntersectionObserver === 'undefined')
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
