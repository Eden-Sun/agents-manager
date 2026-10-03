/**
 * 分享頁的 API client：只打契約 C 的 `/s/{token}/api/…`（API.md §5.7），沒有 UI token、沒有任何主 API 路徑。
 * mock（VITE_MOCK=1）走 `shareMock.ts`，同一個介面。
 */
import { ShareHttpError, toShareFiles, toShareMessage, toSharePage, toStatus, type ShareFile, type ShareMessage, type SharePage, type ShareStatus } from './shareModel'

export interface ShareEvents {
  onMessage: (m: ShareMessage) => void
  onStatus: (s: ShareStatus) => void
  /** 推播斷了（或不支援）：呼叫端改成輪詢。CONNECTING 的重連錯誤也算，不能只等 CLOSED。 */
  onDown: () => void
  /** 連上了：可以停掉輪詢。 */
  onUp?: () => void
}

export interface ShareClient {
  messages(before?: string): Promise<SharePage>
  send(text: string, clientRequestId: string, attachments: string[]): Promise<void>
  upload(file: File): Promise<{ id: string; name: string }>
  files(): Promise<ShareFile[]>
  fileUrl(name: string): string
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
      if (before) q.set('before', before)
      return toSharePage(await json(`/messages?${q}`))
    },
    async send(text, clientRequestId, attachments) {
      await json('/messages', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text, client_request_id: clientRequestId, ...(attachments.length ? { attachments } : {}) }),
      })
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
    subscribe(ev) {
      if (typeof EventSource === 'undefined') {
        ev.onDown()
        return () => {}
      }
      const es = new EventSource(`${base}/events`)
      es.onopen = () => ev.onUp?.()
      es.addEventListener('message', (e) => {
        try {
          const m = toShareMessage(JSON.parse((e as MessageEvent<string>).data))
          if (m) ev.onMessage(m)
        } catch {
          /* 讀不懂的一則略過 */
        }
      })
      es.addEventListener('status', (e) => {
        try {
          ev.onStatus(toStatus(JSON.parse((e as MessageEvent<string>).data)))
        } catch {
          /* ignore */
        }
      })
      // 瀏覽器重連時 readyState 是 CONNECTING，不會先變 CLOSED。任一 error 都改輪詢，
      // 讓撤銷連結的 404 走得到；onopen 再把輪詢停掉。
      es.onerror = () => {
        ev.onDown()
      }
      return () => es.close()
    },
  }
}
