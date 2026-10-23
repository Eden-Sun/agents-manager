import { useEffect, useState } from 'react'
import * as api from '../api'
import type { DeletedBot } from '../api'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { KindTag } from './KindTag'
import { useT } from '../lib/i18n'
import './deletedBotsPanel.css'

function useAgo(): (iso: string) => string {
  const t = useT()
  return (iso: string) => {
    const time = Date.parse(iso)
    if (Number.isNaN(time)) return iso
    const m = Math.max(0, Math.round((Date.now() - time) / 60_000))
    if (m < 1) return t('剛剛')
    if (m < 60) return t('{n} 分鐘前', { n: m })
    if (m < 60 * 24) return t('{n} 小時前', { n: Math.round(m / 60) })
    return t('{n} 天前', { n: Math.round(m / 60 / 24) })
  }
}

/** 環境設定→最近刪除（#757）：軟刪的 bot 與對話都還在，這裡列出來、一鍵復原——不再只靠刪除當下 15 秒的通知。 */
export function DeletedBotsPanel() {
  const t = useT()
  const ago = useAgo()
  const restoreBot = useStore((s) => s.restoreBot)
  // bots 變了（本分頁復原、別的分頁／裝置刪或復原後 refreshState）就重抓，清單跟著同步。
  const bots = useStore((s) => s.bots)
  const [rows, setRows] = useState<DeletedBot[] | null>(null)
  const [failed, setFailed] = useState(false)
  const [busy, setBusy] = useState<string | null>(null)

  useEffect(() => {
    let live = true
    api
      .fetchDeletedBots()
      .then((r) => {
        if (!live) return
        setRows(r)
        setFailed(false)
      })
      .catch(() => live && setFailed(true))
    return () => {
      live = false
    }
  }, [bots])

  if (failed && rows === null) return <p className="deleted-note">{t('讀不到已刪除的清單，稍後再開一次。')}</p>
  if (rows === null) return <p className="deleted-note">{t('載入中…')}</p>
  if (rows.length === 0) return <p className="deleted-note">{t('沒有已刪除的 Bot。')}</p>
  return (
    <div className="deleted-bots">
      <p className="deleted-note">{t('刪除是軟的：設定與對話都還在，按「復原」就回到原本的專案。')}</p>
      <ul className="deleted-list">
        {rows.map((b) => (
          <li key={b.id} className="deleted-row">
            <KindTag kind={b.kind as BotKind} />
            <span className="deleted-main">
              <span className="deleted-name">{b.name}</span>
              <span className="deleted-meta">
                {t('{project} · 刪除於 {ago}', { project: b.project_label, ago: ago(b.deleted_at) })}
                {b.last_message_at ? t(' · 最後對話 {ago}', { ago: ago(b.last_message_at) }) : ''}
              </span>
            </span>
            <button
              type="button"
              className="deleted-restore"
              disabled={busy !== null}
              onClick={async () => {
                setBusy(b.id)
                try {
                  await restoreBot(b.id)
                } finally {
                  setBusy(null)
                }
              }}
            >
              {t('復原')}
            </button>
          </li>
        ))}
      </ul>
    </div>
  )
}
