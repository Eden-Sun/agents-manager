/**
 * 分享頁的 API client：只打契約 C 的 `/s/{token}/api/…`（API.md §5.7），沒有 UI token、沒有任何主 API 路徑。
 * mock（VITE_MOCK=1）走 `shareMock.ts`，同一個介面。
 */
import { ShareHttpError, shareBeforeCursor, toShareFiles, toShareMessage, toSharePage, toStatus, type ShareFile, type ShareMessage, type SharePage, type ShareStatus } from './shareModel'

export interface ShareEvents {
  onMessage: (m: ShareMessage) => void
  onStatus: (s: ShareStatus) => void
  /** 推播斷了（或不支援）：呼叫端改成輪詢。CONNECTING 的重連錯誤也算，不能只等 CLOSED。 */
  onDown: () => void
  /** 漏了事件或對話被倒回：整頁重抓並取代手上的清單。 */
  onResync?: () => void
  /** 連上了：可以停掉輪詢。 */
  onUp?: () => void
}

/** `POST /messages` 的回應（API.md §5.6）：`delivery` 是 bot 到底有沒有收到（`failed`／`unknown` 要讓人知道），`messageId` 是這一則 user 訊息的 id。 */
export interface ShareSendResult {
  delivery?: string
  messageId?: string
}

export interface ShareClient {
  messages(before?: string): Promise<SharePage>
  /** 回應可省（舊 client、mock）：呼叫端把 `undefined` 當成「照舊，沒有額外資訊」。 */
  send(text: string, clientRequestId: string, attachments: string[]): Promise<ShareSendResult | void>
  upload(file: File): Promise<{ id: string; name: string }>
  files(): Promise<ShareFile[]>
  fileUrl(name: string): string
  /** 圖片預覽用的網址（`?inline=1`，只有圖片類 daemon 才回 inline）；`version` 變了瀏覽器才重抓。 */
  previewUrl(name: string, version?: string): string
  /** 檔案內容（SVG 轉 PNG、手機分享用）。 */
  fileBlob(name: string): Promise<Blob>
  subscribe(ev: ShareEvents): () => void
}

async function check(res: Response): Promise<Response> {
  if (res.ok) return res
  const ra = Number(res.headers.get('Retry-After'))
  throw new ShareHttpError(res.status, Number.isFinite(ra) && ra > 0 ? ra : null)
}

export function httpShareClient(token: string): ShareClient {
  const base = `/s/${encodeURIComponent(token)}/api`
  // 不帶 cookie、不帶 referrer：分享頁跟主 UI 沒有任何共用的身分。
  const init: RequestInit = { credentials: 'omit', referrerPolicy: 'no-referrer', cache: 'no-store' }
  const json = async (path: string, extra: RequestInit = {}) => (await check(await fetch(`${base}${path}`, { ...init, ...extra }))).json() as Promise<unknown>
  return {
    async messages(before) {
      const q = new URLSearchParams({ limit: '100' })
      const cursor = shareBeforeCursor(before)
      if (cursor) q.set('before', cursor)
      return toSharePage(await json(`/messages?${q}`))
    },
    async send(text, clientRequestId, attachments) {
      const res = await json('/messages', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text, client_request_id: clientRequestId, ...(attachments.length ? { attachments } : {}) }),
      })
      const o = res && typeof res === 'object' ? (res as Record<string, unknown>) : {}
      return {
        delivery: typeof o.delivery === 'string' ? o.delivery : undefined,
        messageId: typeof o.message_id === 'string' && o.message_id ? o.message_id : undefined,
      }
    },
    async upload(file) {
      const fd = new FormData()
      fd.append('file', file, file.name)
      const o = (await json('/upload', { method: 'POST', body: fd })) as Record<string, unknown>
      return { id: String(o.id ?? ''), name: typeof o.name === 'string' ? o.name : file.name }
    },
    async files() {
      return toShareFiles(await json('/files'))
    },
    fileUrl: (name) => `${base}/files/${encodeURIComponent(name)}`,
    previewUrl: (name, version) => {
      const q = new URLSearchParams({ inline: '1' })
      if (version) q.set('v', version)
      return `${base}/files/${encodeURIComponent(name)}?${q}`
    },
    async fileBlob(name) {
      return (await check(await fetch(`${base}/files/${encodeURIComponent(name)}`, init))).blob()
    },
    subscribe(ev) {
      if (typeof EventSource === 'undefined') {
        ev.onDown()
        return () => {}
      }
      let stopped = false
      let es: EventSource | null = null
      let timer: ReturnType<typeof setTimeout> | null = null
      const bind = (sock: EventSource) => {
        sock.onopen = () => ev.onUp?.()
        sock.addEventListener('message', (e) => {
          try {
            const m = toShareMessage(JSON.parse((e as MessageEvent<string>).data))
            if (m) ev.onMessage(m)
          } catch {
            /* 讀不懂的一則略過 */
          }
        })
        sock.addEventListener('status', (e) => {
          try {
            ev.onStatus(toStatus(JSON.parse((e as MessageEvent<string>).data)))
          } catch {
            /* ignore */
          }
        })
        sock.addEventListener('resync', () => ev.onResync?.())
        // 瀏覽器自己的重連可以緊到 retry:0。關掉這條，4 秒後才再開一條，同時改輪詢。
        sock.onerror = () => {
          ev.onDown()
          sock.close()
          if (stopped || timer !== null) return
          timer = setTimeout(() => {
            timer = null
            open()
          }, 4000)
        }
      }
      const open = () => {
        if (stopped) return
        es = new EventSource(`${base}/events`)
        bind(es)
      }
      open()
      return () => {
        stopped = true
        if (timer !== null) clearTimeout(timer)
        es?.close()
      }
    },
  }
}
