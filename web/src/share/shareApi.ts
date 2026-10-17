/**
 * 分享頁的 API client：只打契約 C 的 `/s/{token}/api/…`（API.md §5.7），沒有 UI token、沒有任何主 API 路徑。
 * mock（VITE_MOCK=1）走 `shareMock.ts`，同一個介面。
 */
import { ShareHttpError, shareBeforeCursor, toShareFiles, toShareMessage, toSharePage, toStatus, type ShareFile, type ShareMessage, type SharePage, type ShareStatus } from './shareModel'

/** 單一 GET（headers 加讀完 body）等太久：放棄，不重送（只有 GET 有期限；POST 不動）。 */
export class ShareTimeoutError extends Error {
  constructor() {
    super('分享頁請求逾時：伺服器沒有在期限內回應')
  }
}

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
  /** `signal` 中止時 fetch 跟著中止（#1661：移除正在傳的附件）。 */
  upload(file: File, signal?: AbortSignal): Promise<{ id: string; name: string }>
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
  // 400 的 body 帶 `reason`（例如 `unknown_attachment`）：文案要分得出來，不然「再試一次」永遠是同一個錯（#1097）。
  let reason: string | null = null
  if (res.status === 400) {
    try {
      const b = (await res.json()) as { reason?: unknown }
      if (typeof b?.reason === 'string') reason = b.reason
    } catch {
      /* 讀不懂就當沒有原因 */
    }
  }
  throw new ShareHttpError(res.status, Number.isFinite(ra) && ra > 0 ? ra : null, reason)
}

/** 下載名額每個分享同時 2 個、不排隊（portal.rs `DOWNLOADS_PER_SHARE`）：整檔下載一次只發一個，留一格給 `<img>` 預覽；429／503 照 `Retry-After` 等了重試（#1089）。 */
const BLOB_RETRY_MAX = 6
const BLOB_WAIT_MAX_S = 10

/** GET 與檔案下載的期限（#1804）：涵蓋 headers 與讀完 body；到期就 abort，讓 `fileBlob` 的串列往下走。 */
const GET_DEADLINE_MS = 30_000
const BLOB_DEADLINE_MS = 60_000

export interface ShareClientOptions {
  /** 等多久（測試換掉）。 */
  sleep?: (ms: number) => Promise<void>
  /** 期限計時器（測試換成手動觸發的，不靠牆鐘）。 */
  timers?: { setTimeout: (fn: () => void, ms: number) => unknown; clearTimeout: (id: unknown) => void }
}

/**
 * 整段（fetch、讀 body）的期限：到期時 abort 並以 `ShareTimeoutError` 結束。傳進去的 `run` 不一定理 signal，
 * 所以這裡自己跟 abort 賽跑，不會因為某個 fetch 或 body 永不結束而掛住。
 */
function withDeadline<T>(timers: NonNullable<ShareClientOptions['timers']>, ms: number, run: (signal: AbortSignal) => Promise<T>): Promise<T> {
  const ctl = new AbortController()
  const expired = new Promise<never>((_, reject) => {
    ctl.signal.addEventListener('abort', () => reject(new ShareTimeoutError()))
  })
  expired.catch(() => {})
  const id = timers.setTimeout(() => ctl.abort(), ms)
  return Promise.race([run(ctl.signal), expired]).finally(() => timers.clearTimeout(id))
}

export function httpShareClient(token: string, opts: ShareClientOptions = {}): ShareClient {
  const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)))
  const timers = opts.timers ?? { setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms), clearTimeout: (id: unknown) => clearTimeout(id as ReturnType<typeof setTimeout>) }
  const base = `/s/${encodeURIComponent(token)}/api`
  // 不帶 cookie、不帶 referrer：分享頁跟主 UI 沒有任何共用的身分。
  const init: RequestInit = { credentials: 'omit', referrerPolicy: 'no-referrer', cache: 'no-store' }
  const json = async (path: string, extra: RequestInit = {}) => (await check(await fetch(`${base}${path}`, { ...init, ...extra }))).json() as Promise<unknown>
  const getJson = (path: string) => withDeadline(timers, GET_DEADLINE_MS, async (signal) => (await check(await fetch(`${base}${path}`, { ...init, signal }))).json() as Promise<unknown>)
  const fetchBlob = async (name: string): Promise<Blob> => {
    for (let attempt = 0; ; attempt++) {
      try {
        return await withDeadline(timers, BLOB_DEADLINE_MS, async (signal) => (await check(await fetch(`${base}/files/${encodeURIComponent(name)}`, { ...init, signal }))).blob())
      } catch (e) {
        if (!(e instanceof ShareHttpError) || (e.status !== 429 && e.status !== 503) || attempt >= BLOB_RETRY_MAX) throw e
        await sleep(Math.min(e.retryAfter ?? 2, BLOB_WAIT_MAX_S) * 1000)
      }
    }
  }
  let blobChain: Promise<unknown> = Promise.resolve()
  return {
    async messages(before) {
      const q = new URLSearchParams({ limit: '100' })
      const cursor = shareBeforeCursor(before)
      if (cursor) q.set('before', cursor)
      return toSharePage(await getJson(`/messages?${q}`))
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
    async upload(file, signal) {
      const fd = new FormData()
      fd.append('file', file, file.name)
      const o = (await json('/upload', { method: 'POST', body: fd, signal })) as Record<string, unknown>
      return { id: String(o.id ?? ''), name: typeof o.name === 'string' ? o.name : file.name }
    },
    async files() {
      return toShareFiles(await getJson('/files'))
    },
    fileUrl: (name) => `${base}/files/${encodeURIComponent(name)}`,
    previewUrl: (name, version) => {
      const q = new URLSearchParams({ inline: '1' })
      if (version) q.set('v', version)
      return `${base}/files/${encodeURIComponent(name)}?${q}`
    },
    fileBlob(name) {
      const run = blobChain.then(() => fetchBlob(name))
      // 前一個失敗不卡住後面的。
      blobChain = run.catch(() => {})
      return run
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
