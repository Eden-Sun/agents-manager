import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import type { OutboxFile } from '../api'
import { isHtmlFile } from '../lib/htmlPreview'
import { emptyReason, fileSize, isBrowserPreviewable, isPreviewableImage, LOAD_FAILED, outboxExtTone, remainingLabel, remainingNow, splitOutboxExt, splitOutboxName } from '../lib/outboxList'
import { OutboxImagePreview } from './OutboxImagePreview'
import { useOutboxFiles } from './useOutboxFiles'
import './outboxFiles.css'

export function OutboxFiles() {
  const t = useT()
  const { botId, files, reason, dir, kept, fetchedAt, now, loadedFor, busy, refresh, download, openPreview } = useOutboxFiles()
  const [peek, setPeek] = useState<{ name: string; owner?: string; modified: number; anchor: DOMRect } | null>(null)
  const peekTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const showPeek = (f: OutboxFile, el: HTMLElement) => {
    if (!isPreviewableImage(f.name)) return
    if (peekTimer.current) clearTimeout(peekTimer.current)
    peekTimer.current = setTimeout(() => setPeek({ name: f.name, owner: f.owner, modified: f.modified, anchor: el.getBoundingClientRect() }), 150)
  }
  const hidePeek = () => {
    if (peekTimer.current) clearTimeout(peekTimer.current)
    peekTimer.current = null
    setPeek(null)
  }

  const [peekBot, setPeekBot] = useState(botId)
  if (peekBot !== botId) {
    setPeekBot(botId)
    setPeek(null)
  }
  useEffect(
    () => () => {
      if (peekTimer.current) clearTimeout(peekTimer.current)
    },
    [botId],
  )

  const loading = Boolean(botId) && loadedFor !== botId
  const quietEmpty = files.length === 0 && !loading && Boolean(botId) && reason === null
  const headTip = quietEmpty ? (dir ? `${emptyReason(null, true, kept)}\n${dir}` : emptyReason(null, true, kept)) : dir || undefined

  return (
    <section className={`outbox-files${quietEmpty ? ' compact' : ''}`} aria-label={t("這顆 bot 交給你的檔案")}>
      <div className="outbox-head">
        <span className="outbox-title" title={headTip}>
          {t('bot 給你的檔案')}
        </span>
        {files.length > 0 ? <span className="outbox-count">{files.length}</span> : null}
        {quietEmpty ? (
          <span className="outbox-empty-inline" title={headTip}>
            {t('無檔案')}
          </span>
        ) : null}
        <span className="spacer" />
        <button
          type="button"
          className="icon-btn outbox-refresh"
          disabled={!botId || loadedFor !== botId}
          aria-label={t("重新讀取 bot 給你的檔案")}
          title={t("重新讀取（bot 剛放進去的檔案按這裡才會出現）")}
          onClick={refresh}
        >
          ↻
        </button>
      </div>
      {quietEmpty ? null : files.length === 0 ? (
        <p className={`outbox-empty${reason === LOAD_FAILED ? ' failed' : ''}`} role={reason === LOAD_FAILED ? 'alert' : undefined}>
          {loading ? t('讀取中…') : emptyReason(reason, Boolean(botId), kept)}
        </p>
      ) : (
        <ul className="outbox-list" role="list">
          {files.map((f) => {
            const left = remainingNow(f, fetchedAt, now)
            const { dir: folder, base } = splitOutboxName(f.name)
            const { stem, ext } = splitOutboxExt(base)
            const itemBusy = busy?.bot === (f.owner ?? botId) && busy.name === f.name ? busy.mode : null
            const isBusy = itemBusy !== null
            const canPreview = isBrowserPreviewable(f.name) || isHtmlFile(f.name)
            return (
              <li key={`${f.owner ?? ''}/${f.name}`} className="outbox-item">
                <button
                  type="button"
                  className="outbox-file"
                  disabled={isBusy}
                  title={t("下載 {name}", { name: f.name })}
                  onClick={() => void download(f.name, f.owner)}
                  onMouseEnter={(e) => showPeek(f, e.currentTarget)}
                  onMouseLeave={hidePeek}
                  onFocus={(e) => showPeek(f, e.currentTarget)}
                  onBlur={hidePeek}
                >
                  <span className="outbox-name">
                    {folder ? <span className="outbox-dir">{folder}</span> : null}
                    {stem}
                  </span>
                  <span className={`outbox-meta${left !== null && left <= 600 ? ' soon' : ''}`}>
                    {ext ? <span className={`outbox-ext outbox-ext-${outboxExtTone(ext)}`}>{ext.toUpperCase()}</span> : null}
                    <span className="outbox-meta-text">
                      {fileSize(f.size)}
                      {f.ownerName ? t(' · 來自 {name}', { name: f.ownerName }) : ''}
                      {left === null ? '' : ` · ${remainingLabel(left)}`}
                      {itemBusy === 'preview' ? t(' · 開啟中…') : itemBusy === 'download' ? t(' · 下載中…') : ''}
                    </span>
                  </span>
                </button>
                {canPreview ? (
                  <button
                    type="button"
                    className="icon-btn outbox-preview-btn"
                    disabled={isBusy}
                    aria-label={t("開新分頁預覽 {name}", { name: f.name })}
                    title={t("開新分頁預覽（不下載）")}
                    onClick={(e) => {
                      e.stopPropagation()
                      void openPreview(f.name, f.owner)
                    }}
                  >
                    ↗
                  </button>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
      {peek && botId ? <OutboxImagePreview botId={peek.owner ?? botId} name={peek.name} modified={peek.modified} anchor={peek.anchor} /> : null}
    </section>
  )
}
