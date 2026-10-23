import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { readableImagePath } from '../lib/markdownUrl'
import { fetchRemoteImageBlob, isRemoteHttpImage, remoteImageInfo } from '../lib/remoteImage'
import './markdownImage.css'

/** 網址型的圖片照常用 `<img>`；其他（`docs/x.png`、`/Users/…/x.png`、`file://…`）當成 bot 專案裡的檔案。 */
function isRemote(src: string): boolean {
  return /^(https?:|data:|blob:)/i.test(src)
}

/** 沒寫替代文字（`![](x.png)`）時的名字：檔名。圖片包在連結裡，alt 是空的，連結就沒有任何可讀名稱（#1637）。 */
function imageName(src: string): string {
  if (/^(data:|blob:)/i.test(src)) return 'image' // i18n-ignore
  const last = src.split(/[?#]/)[0].split(/[\\/]/).pop() ?? ''
  try {
    return decodeURIComponent(last) || 'image' // i18n-ignore
  } catch {
    return last || 'image' // i18n-ignore
  }
}

/**
 * 遠端 http(s) 圖片預設不載入（issue #764）：先畫佔位（網域＋「載入圖片」），點了才抓這一張。
 * 抓的方式是 `fetch`→blob（`index.html` 的 CSP 只放 `img-src 'self' data: blob:`，`<img src="https://…">` 本來就載不了）；
 * 對方沒開 CORS 或抓不到，就改成「在新分頁開啟」的連結（那是使用者自己點的連結，不是自動請求）。
 */
function RemoteImage({ src, alt }: { src: string; alt?: string }) {
  const t = useT()
  const info = remoteImageInfo(src)
  const [state, setState] = useState<'idle' | 'loading' | 'failed'>('idle')
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => () => {
    if (url) URL.revokeObjectURL(url)
  }, [url])
  // 載入途中元件被卸掉：晚到的 blob 不能留下沒人回收的 object URL。
  const alive = useRef(true)
  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])
  if (url) {
    return (
      <a href={url} target="_blank" rel="noreferrer" className="md-image-link">
        <img className="md-image" src={url} alt={alt || imageName(src)} />
      </a>
    )
  }
  const host = info?.host ?? t('未知網域')
  const load = () => {
    setState('loading')
    fetchRemoteImageBlob(src)
      .then((blob) => {
        const made = URL.createObjectURL(blob)
        if (alive.current) setUrl(made)
        else URL.revokeObjectURL(made)
      })
      .catch(() => setState('failed'))
  }
  return (
    <span className="md-image-remote" title={src}>
      <span aria-hidden="true">🖼</span> {alt ? t.x('外部圖片（{host}）：{alt}', { host: <strong>{host}</strong>, alt }) : t.x('外部圖片（{host}）', { host: <strong>{host}</strong> })}
      {info?.suspicious ? <span className="md-image-warn"> {t('網址帶了很長的參數或帳密，可能夾帶資料，確定要載入再點。')}</span> : null}
      {info?.internal ? <span className="md-image-warn"> {t('這是本機／內網位址：點下去會由這台機器對它發出請求。')}</span> : null}
      {state === 'failed' ? (
        <>
          <span className="md-image-warn"> {t('載入失敗（對方不允許跨站讀取、轉址到別的主機、不是支援的點陣圖或太大）。')}</span>
          <a href={src} target="_blank" rel="noreferrer noopener">
            {t('在新分頁開啟')}
          </a>
        </>
      ) : (
        <button
          type="button"
          className="mini-btn"
          disabled={state === 'loading'}
          onClick={(e) => {
            // 可能被包在 Markdown 連結裡：按鈕只負責抓這張圖，不能順便把外層連結開出去（#1238）。
            e.preventDefault()
            e.stopPropagation()
            load()
          }}
        >
          {state === 'loading' ? t('載入中…') : t('載入圖片')}
        </button>
      )}
    </span>
  )
}

/**
 * bot 回覆裡的 `![](docs/shot.png)`（2026-09-15 使用者截圖：只剩破圖）。路徑是 bot 工作目錄裡的檔，瀏覽器讀不到，
 * 改由 daemon 從該 bot 的專案目錄帶權杖讀出來（`GET /api/bots/{id}/local-image`）。讀不到就把路徑直接寫出來，不畫破圖。
 */
export function MarkdownImage({
 botId, src, alt }: { botId: string | null | undefined; src?: string; alt?: string }) {
  const t = useT()
  const raw = typeof src === 'string' ? src : ''
  const local = raw !== '' && !isRemote(raw)
  // 結果綁定來源（botId＋raw）：換了來源，上一張的 URL 或失敗狀態不沿用，要等新來源自己的結果（#1801）。
  const scope = `${botId ?? ''}\n${raw}`
  const [result, setResult] = useState<{ scope: string; url?: string; failed?: boolean }>({ scope: '' })
  const current = result.scope === scope ? result : null
  const url = current?.url ?? null
  const failed = current?.failed ?? false

  useEffect(() => {
    if (!local || !botId) return
    let alive = true
    let made: string | null = null
    api
      .localImageUrl(botId, raw)
      .then((u) => {
        made = u
        if (alive) setResult({ scope, url: u })
        else URL.revokeObjectURL(u)
      })
      .catch(() => {
        if (alive) setResult({ scope, failed: true })
      })
    return () => {
      alive = false
      if (made) URL.revokeObjectURL(made)
      // 離開這個來源就把它的結果清掉：blob 已經 revoke，失敗狀態也不該留給之後換回來的同一個來源（review #1801）。
      setResult({ scope: '' })
    }
  }, [local, botId, raw, scope])

  if (!raw) return null
  if (!local) {
    // key＝src：換了網址就整個重來，上一張的授權結果與 blob 不沿用，新網址要重新手動按「載入圖片」。
    if (isRemoteHttpImage(raw)) return <RemoteImage key={raw} src={raw} alt={alt} />
    return <img className="md-image" src={raw} alt={alt || imageName(raw)} loading="lazy" />
  }
  if (failed || !botId) {
    return (
      <span className="md-image-missing" title={t("圖片不在這個專案目錄裡、不是圖片檔，或檔案不存在")}>
        🖼 {alt ? t('{alt}：', { alt }) : ''}
        <code>{readableImagePath(raw)}</code>
      </span>
    )
  }
  if (!url) return <span className="md-image-loading">🖼 {alt || raw}</span>
  return (
    <a href={url} target="_blank" rel="noreferrer" className="md-image-link">
      <img className="md-image" src={url} alt={alt || imageName(raw)} />
    </a>
  )
}
