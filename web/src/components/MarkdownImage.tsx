import { useEffect, useState } from 'react'
import * as api from '../api'
import { readableImagePath } from '../lib/markdownUrl'
import './markdownImage.css'

/** 網址型的圖片照常用 `<img>`；其他（`docs/x.png`、`/Users/…/x.png`、`file://…`）當成 bot 專案裡的檔案。 */
function isRemote(src: string): boolean {
  return /^(https?:|data:|blob:)/i.test(src)
}

/**
 * bot 回覆裡的 `![](docs/shot.png)`（2026-09-15 使用者截圖：只剩破圖）。路徑是 bot 工作目錄裡的檔，瀏覽器讀不到，
 * 改由 daemon 從該 bot 的專案目錄帶權杖讀出來（`GET /api/bots/{id}/local-image`）。讀不到就把路徑直接寫出來，不畫破圖。
 */
export function MarkdownImage({ botId, src, alt }: { botId: string | null | undefined; src?: string; alt?: string }) {
  const raw = typeof src === 'string' ? src : ''
  const local = raw !== '' && !isRemote(raw)
  const [url, setUrl] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    if (!local || !botId) return
    let alive = true
    let made: string | null = null
    api
      .localImageUrl(botId, raw)
      .then((u) => {
        made = u
        if (alive) setUrl(u)
        else URL.revokeObjectURL(u)
      })
      .catch(() => {
        if (alive) setFailed(true)
      })
    return () => {
      alive = false
      if (made) URL.revokeObjectURL(made)
    }
  }, [local, botId, raw])

  if (!raw) return null
  if (!local) return <img className="md-image" src={raw} alt={alt ?? ''} loading="lazy" />
  if (failed || !botId) {
    return (
      <span className="md-image-missing" title="圖片不在這個專案目錄裡、不是圖片檔，或檔案不存在">
        🖼 {alt ? `${alt}：` : ''}
        <code>{readableImagePath(raw)}</code>
      </span>
    )
  }
  if (!url) return <span className="md-image-loading">🖼 {alt || raw}</span>
  return (
    <a href={url} target="_blank" rel="noreferrer" className="md-image-link">
      <img className="md-image" src={url} alt={alt ?? ''} />
    </a>
  )
}
