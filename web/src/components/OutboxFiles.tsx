import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import type { OutboxFile } from '../api'
import { emptyReason, fileSize, isBrowserPreviewable, isPreviewableImage, LOAD_FAILED, remainingLabel, remainingNow, splitOutboxName } from '../lib/outboxList'
import { OutboxImagePreview } from './OutboxImagePreview'
import { useOutboxFiles } from './useOutboxFiles'
import './outboxFiles.css'

/**
 * 檔案暫存區裡的「{t("bot 給你的檔案")}」：bot 把要交給使用者的東西放進 `$AM_OUTBOX`（SPEC §6.5f），
 * 使用者在這裡點一下就下載得到——那些檔案在 daemon 這台機器上，手機上本來拿不到。
 *
 * **只讀 outbox，不讀 scratchpad**（使用者 2026-09-16 裁示：scratchpad 暴露過私鑰與正式 DB 複本）。
 * 檔案放進去 1 小時後由 AGM 清掉，所以每個檔案標出還剩多久；分享用 bot（SPEC §20）的不清，不畫倒數。
 *
 * 只讀：這一段不刪檔、不改檔。上面那半（拖進來的暫存）是要**送進**對話的東西，這半是**從** bot 拿出來的。
 */
export function OutboxFiles() {
  const t = useT()
  const { botId, files, reason, dir, kept, fetchedAt, now, loadedFor, busy, refresh, download, openPreview } = useOutboxFiles()
  /** 滑過（或鍵盤聚焦）的圖檔：浮出預覽。 */
  const [peek, setPeek] = useState<{ name: string; modified: number; anchor: DOMRect } | null>(null)
  const peekTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const showPeek = (f: OutboxFile, el: HTMLElement) => {
    if (!isPreviewableImage(f.name)) return
    if (peekTimer.current) clearTimeout(peekTimer.current)
    // 滑鼠只是劃過清單不該每張都抓一次：停 150ms 才算要看。
    peekTimer.current = setTimeout(() => setPeek({ name: f.name, modified: f.modified, anchor: el.getBoundingClientRect() }), 150)
  }
  const hidePeek = () => {
    if (peekTimer.current) clearTimeout(peekTimer.current)
    peekTimer.current = null
    setPeek(null)
  }

  // 換 bot：浮出的預覽跟著收掉（清單本身由 hook 在 render 當下清）。
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

  return (
    <section className="outbox-files" aria-label={t("這顆 bot 交給你的檔案")}>
      <div className="outbox-head">
        <span className="outbox-title" title={dir || undefined}>
          {t('bot 給你的檔案')}
        </span>
        {files.length > 0 ? <span className="outbox-count">{files.length}</span> : null}
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
      {files.length === 0 ? (
        <p className={`outbox-empty${reason === LOAD_FAILED ? ' failed' : ''}`} role={reason === LOAD_FAILED ? 'alert' : undefined}>
          {botId && loadedFor !== botId ? t('讀取中…') : emptyReason(reason, Boolean(botId), kept)}
        </p>
      ) : (
        <ul className="outbox-list" role="list">
          {files.map((f) => {
            const left = remainingNow(f, fetchedAt, now)
            const { dir: folder, base } = splitOutboxName(f.name)
            const itemBusy = busy?.bot === botId && busy.name === f.name ? busy.mode : null
            const isBusy = itemBusy !== null
            const canPreview = isBrowserPreviewable(f.name)
            return (
              <li key={f.name} className="outbox-item">
                <button
                  type="button"
                  className="outbox-file"
                  disabled={isBusy}
                  title={t("下載 {name}", { name: f.name })}
                  onClick={() => void download(f.name)}
                  onMouseEnter={(e) => showPeek(f, e.currentTarget)}
                  onMouseLeave={hidePeek}
                  onFocus={(e) => showPeek(f, e.currentTarget)}
                  onBlur={hidePeek}
                >
                  <span className="outbox-name">
                    {folder ? <span className="outbox-dir">{folder}</span> : null}
                    {base}
                  </span>
                  <span className={`outbox-meta${left !== null && left <= 600 ? ' soon' : ''}`}>
                    {fileSize(f.size)}
                    {left === null ? '' : ` · ${remainingLabel(left)}`}
                    {itemBusy === 'preview' ? t(' · 開啟中…') : itemBusy === 'download' ? t(' · 下載中…') : ''}
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
                      void openPreview(f.name)
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
      {peek && botId ? <OutboxImagePreview botId={botId} name={peek.name} modified={peek.modified} anchor={peek.anchor} /> : null}
    </section>
  )
}
