import { useEffect, useState } from 'react'
import type { OutboxFile } from '../api'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useT } from '../lib/i18n'
import { fileSize, isPreviewableImage, remainingLabel, remainingNow, splitOutboxName } from '../lib/outboxList'
import { blobFor } from '../lib/outboxPreviewCache'
import { Modal } from './Modal'
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
 * 沒有檔案整條不畫；預設收合，有新檔只在列上的數字標出來，不自動展開。清單、下載、開新分頁預覽與桌面托盤共用
 * `useOutboxFiles`；縮圖與全螢幕預覽共用 `outboxPreviewCache` 的 blob 快取。
 */
export function PhoneOutboxTray() {
  const phone = useMediaQuery(PHONE_QUERY)
  // 桌面不掛：清單由右側欄的托盤讀，這裡不必多讀一份。
  return phone ? <PhoneOutboxTrayInner /> : null
}

function PhoneOutboxTrayInner() {
  const t = useT()
  const { botId, files, fetchedAt, now, loadedFor, busy, download } = useOutboxFiles()
  const [open, setOpen] = useState(readOpen)
  const [viewing, setViewing] = useState<OutboxFile | null>(null)
  /** 看過幾個檔：之後多出來的在列上標「+N 新」。`null`＝這顆 bot 的清單還沒讀到；第一次讀到的那批不算新。 */
  const [seen, setSeen] = useState<{ bot: string | null; count: number | null }>({ bot: botId, count: null })
  if (seen.bot !== botId) {
    setSeen({ bot: botId, count: null })
    setViewing(null)
  } else if (loadedFor === botId) {
    // 第一次讀到、展開著（進來的檔使用者看得到）、或檔案被清掉（數字往下走）：都把看過的數對齊目前清單。
    if (seen.count === null || (open && seen.count !== files.length) || seen.count > files.length) setSeen({ bot: botId, count: files.length })
  }

  if (!botId || files.length === 0) return null

  const fresh = open || seen.count === null ? 0 : files.length - seen.count
  const images = files.filter((f) => isPreviewableImage(f.name))
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
            <ul className="phone-outbox-thumbs" role="list">
              {images.map((f) => (
                <li key={f.name}>
                  <button
                    type="button"
                    className="phone-outbox-thumb"
                    aria-label={t('預覽 {name}', { name: f.name })}
                    onClick={() => setViewing(f)}
                  >
                    <OutboxThumb botId={botId} file={f} />
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
          <ul className="phone-outbox-list" role="list">
            {files.map((f) => {
              const left = remainingNow(f, fetchedAt, now)
              const { dir: folder, base } = splitOutboxName(f.name)
              const isBusy = busy?.bot === botId && busy.name === f.name
              const image = isPreviewableImage(f.name)
              return (
                <li key={f.name} className="phone-outbox-item">
                  <button
                    type="button"
                    className="outbox-file"
                    disabled={isBusy}
                    // 圖檔點名字開預覽（預覽裡有下載鈕）；其他照原本點一下就下載。
                    title={image ? t('預覽 {name}', { name: f.name }) : t('下載 {name}', { name: f.name })}
                    onClick={() => (image ? setViewing(f) : void download(f.name))}
                  >
                    <span className="outbox-name">
                      {folder ? <span className="outbox-dir">{folder}</span> : null}
                      {base}
                    </span>
                    <span className={`outbox-meta${left !== null && left <= 600 ? ' soon' : ''}`}>
                      {fileSize(f.size)}
                      {left === null ? '' : ` · ${remainingLabel(left)}`}
                      {isBusy ? t(' · 下載中…') : ''}
                    </span>
                  </button>
                  <button
                    type="button"
                    className="icon-btn phone-outbox-dl"
                    disabled={isBusy}
                    aria-label={t('下載 {name}', { name: f.name })}
                    onClick={() => void download(f.name)}
                  >
                    ⤓
                  </button>
                </li>
              )
            })}
          </ul>
        </div>
      ) : null}
      {viewing ? (
        <Modal open title={splitOutboxName(viewing.name).base} width={960} onClose={() => setViewing(null)}>
          <div className="phone-outbox-viewer">
            <OutboxThumb botId={botId} file={viewing} full />
            <button
              type="button"
              className="btn primary"
              disabled={busy?.bot === botId && busy.name === viewing.name}
              onClick={() => void download(viewing.name)}
            >
              {t('下載')}
            </button>
          </div>
        </Modal>
      ) : null}
    </section>
  )
}

/** 縮圖與全螢幕預覽都走 `blobFor`：同一張圖只抓一次（token 帶不進 `<img src>`）。 */
function OutboxThumb({ botId, file, full = false }: { botId: string; file: OutboxFile; full?: boolean }) {
  const t = useT()
  const key = `${botId}/${file.name}/${file.modified}`
  const [src, setSrc] = useState<{ key: string; url: string | null }>({ key: '', url: null })
  useEffect(() => {
    let alive = true
    blobFor(botId, file.name, file.modified).then(
      (url) => alive && setSrc({ key, url }),
      () => alive && setSrc({ key, url: null }),
    )
    return () => {
      alive = false
    }
  }, [botId, file.name, file.modified, key])
  const cls = full ? 'phone-outbox-full' : 'phone-outbox-thumb-img'
  if (src.key !== key) return <span className={`${cls} note`}>{full ? t('載入預覽…') : '…'}</span>
  if (!src.url) return <span className={`${cls} note`}>{full ? t('預覽載不出來') : '✕'}</span>
  return <img className={cls} src={src.url} alt={file.name} />
}
