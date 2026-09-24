import { ApiError } from './types'
import type { ApiErrorBody } from './types'

export type HttpMethod = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'

export interface SocketHandlers {
  onFrame: (frame: { seq?: number; type: string; data?: unknown }) => void
  onStatus: (status: 'connecting' | 'open' | 'closed') => void
  /** Highest processed seq; sent as `?since=`. */
  since: () => number
}

export interface Transport {
  readonly mock: boolean
  session: () => Promise<string>
  /** `signal`：呼叫端自己決定要不要逾時／中止；transport 本身不設逾時。 */
  request: (method: HttpMethod, path: string, body?: unknown, signal?: AbortSignal) => Promise<unknown>
  /** 附件上傳：要進度與中止，所以走 XHR（fetch 拿不到 request body 的進度）。 */
  upload: (path: string, file: Blob, opts?: UploadOptions) => Promise<unknown>
  /** Attachments need the token header, which `<img src>` can't carry — so fetch and blob-URL. */
  blobUrl: (path: string) => Promise<string>
  openSocket: (handlers: SocketHandlers) => () => void
}

export interface UploadOptions {
  /** 中止：真的 `xhr.abort()`，promise 以 `AbortError` reject（呼叫端據此不當成失敗）。 */
  signal?: AbortSignal
  /** 已送出／總位元組（request body 的，瀏覽器量的）。 */
  onProgress?: (loaded: number, total: number) => void
}

export function abortError(): DOMException {
  return new DOMException('上傳已取消', 'AbortError')
}

export function isAbortError(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { name?: unknown }).name === 'AbortError'
}

function parseText(status: number, text: string): unknown {
  if (status === 204 || !text) return null
  try {
    return JSON.parse(text) as unknown
  } catch {
    return text
  }
}

/**
 * `POST` 一個 Blob，回 HTTP 狀態與解析過的 body；網路斷掉 reject `Error`、中止 reject `AbortError`。
 * 不設逾時（跟原本的 fetch 一樣）：50 MB 在慢網路上本來就要很久，卡住時卡片上有進度可看、× 會真的中止。
 */
export function xhrUpload(
  url: string,
  file: Blob,
  token: string,
  opts: UploadOptions = {},
  create: () => XMLHttpRequest = () => new XMLHttpRequest(),
): Promise<{ status: number; body: unknown }> {
  const { signal, onProgress } = opts
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(abortError())
      return
    }
    const xhr = create()
    const onAbort = () => xhr.abort()
    const settle = () => signal?.removeEventListener('abort', onAbort)
    xhr.open('POST', url)
    xhr.setRequestHeader('Accept', 'application/json')
    if (token) xhr.setRequestHeader('X-AM-Token', token)
    xhr.setRequestHeader('Content-Type', file.type || 'application/octet-stream')
    if (onProgress) {
      xhr.upload.onprogress = (e: ProgressEvent) => onProgress(e.loaded, e.lengthComputable ? e.total : file.size)
    }
    xhr.onload = () => {
      settle()
      resolve({ status: xhr.status, body: parseText(xhr.status, xhr.responseText) })
    }
    xhr.onerror = () => {
      settle()
      reject(new Error('連線中斷，上傳沒有完成'))
    }
    xhr.onabort = () => {
      settle()
      reject(abortError())
    }
    signal?.addEventListener('abort', onAbort, { once: true })
    xhr.send(file)
  })
}

async function readBody(res: Response): Promise<unknown> {
  return res.status === 204 ? null : parseText(res.status, await res.text())
}

export class HttpTransport implements Transport {
  readonly mock = false
  private token = ''
  /** 同時只跑一次 `GET /api/session`：401 常常一次來一整批（終端每秒輪詢＋WS 重連）。 */
  private refreshing: Promise<string> | null = null

  /**
   * token 只在開頁時拿一次、存在記憶體裡：那一次沒拿到（daemon 正在重啟／換版），這一頁之後每個請求都
   * 401，畫面就卡在「讀取終端失敗：missing or bad X-AM-Token」直到使用者自己重新整理
   * （2026-09-20 使用者在 blocked 面板上看到）。所以 401 時重拿一次 token 再重試一次。
   */
  private async refreshToken(): Promise<string> {
    if (!this.refreshing) {
      this.refreshing = this.session().finally(() => {
        this.refreshing = null
      })
    }
    return this.refreshing
  }

  /** 重試過還是 401 就照實丟出去（token 真的不對，重試再多次也一樣）。 */
  private async withFreshToken<T>(send: (tok: string) => Promise<Response>, run: (res: Response) => Promise<T>): Promise<T> {
    let res = await send(this.token)
    if (res.status === 401) {
      const before = this.token
      let tok = ''
      try {
        tok = await this.refreshToken()
      } catch {
        return run(res)
      }
      if (tok && tok !== before) res = await send(tok)
    }
    return run(res)
  }

  async session(): Promise<string> {
    const res = await fetch('/api/session', { headers: { Accept: 'application/json' } })
    const body = await readBody(res)
    if (!res.ok) {
      throw new ApiError(res.status, (body ?? {}) as ApiErrorBody, `GET /api/session failed (${res.status})`)
    }
    const token =
      typeof body === 'string'
        ? body
        : body && typeof body === 'object' && typeof (body as { token?: unknown }).token === 'string'
          ? (body as { token: string }).token
          : ''
    if (!token) throw new Error('GET /api/session returned no token')
    this.token = token
    return token
  }

  async request(method: HttpMethod, path: string, body?: unknown, signal?: AbortSignal): Promise<unknown> {
    const send = (tok: string) => {
      const headers: Record<string, string> = { Accept: 'application/json' }
      if (tok) headers['X-AM-Token'] = tok
      if (body !== undefined) headers['Content-Type'] = 'application/json'
      return fetch(`/api${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal,
      })
    }
    // 只有 GET 敢重試：POST/PATCH 401 時 daemon 根本沒跑到處理函式，但重送一次仍可能重複送出。
    const res = method === 'GET' ? await this.withFreshToken(send, async (r) => r) : await send(this.token)
    const parsed = await readBody(res)
    if (!res.ok) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: String(parsed ?? '') }
      throw new ApiError(res.status, errBody, `${method} ${path} failed (${res.status})`)
    }
    return parsed
  }

  async upload(path: string, file: Blob, opts?: UploadOptions): Promise<unknown> {
    const { status, body: parsed } = await xhrUpload(`/api${path}`, file, this.token, opts)
    if (status < 200 || status >= 300) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: String(parsed ?? '') }
      throw new ApiError(status, errBody, `POST ${path} failed (${status})`)
    }
    return parsed
  }

  async blobUrl(path: string): Promise<string> {
    const send = (tok: string) => fetch(`/api${path}`, { headers: tok ? { 'X-AM-Token': tok } : {} })
    const res = await this.withFreshToken(send, async (r) => r)
    if (!res.ok) {
      // daemon 講的原因在 body 裡（`{error, reason, …}`）。以前只拿 `res.statusText`，畫面上就只剩
      // 「Not Found」／「Conflict」（HTTP/2 連 statusText 都是空字串），連 `file_too_large` 帶的
      // size／max 也一起丟掉（issue #547）。
      const parsed = parseText(res.status, await res.text().catch(() => ''))
      const body: ApiErrorBody =
        parsed && typeof parsed === 'object'
          ? (parsed as ApiErrorBody)
          : { reason: String(parsed ?? '') || res.statusText }
      throw new ApiError(res.status, body, `GET ${path} failed (${res.status})`)
    }
    return URL.createObjectURL(await res.blob())
  }

  openSocket(handlers: SocketHandlers): () => void {
    let closed = false
    let attempt = 0
    let sock: WebSocket | null = null
    let timer: ReturnType<typeof setTimeout> | null = null

    const connect = () => {
      if (closed) return
      // 先拆乾淨舊 socket：CLOSING 的會過 `retryNow` 檢查，其 onclose 晚到會再排 connect，變兩條、frame 收兩次。
      if (sock) {
        const old = sock
        sock = null
        old.onopen = null
        old.onmessage = null
        old.onerror = null
        old.onclose = null
        old.close()
      }
      handlers.onStatus('connecting')
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
      // token 空的時候連了也會被踢掉，先補一次再連（下一輪 backoff 會再試）。
      if (!this.token) {
        void this.refreshToken().then(retryNow, () => {})
      }
      const qs = new URLSearchParams({ token: this.token, since: String(handlers.since()) })
      const ws = new WebSocket(`${scheme}://${location.host}/ws?${qs.toString()}`)
      sock = ws
      ws.onopen = () => {
        if (sock !== ws) return
        attempt = 0
        handlers.onStatus('open')
      }
      ws.onmessage = (ev: MessageEvent<string>) => {
        if (sock !== ws) return
        let frame: { seq?: number; type: string; data?: unknown } | null = null
        try {
          frame = JSON.parse(ev.data) as { seq?: number; type: string; data?: unknown }
        } catch {
          /* malformed */
        }
        if (!frame || typeof frame.type !== 'string') return
        // handler 例外不能吞：`lastSeq` 已推進，重連不會補這則，至少留 console 痕跡。
        try {
          handlers.onFrame(frame)
        } catch (e) {
          console.error(`[ws] handler threw on ${frame.type} frame`, e)
        }
      }
      ws.onerror = () => ws.close()
      ws.onclose = () => {
        // 已被新連線取代就不動（狀態與重連歸新的管）。
        if (closed || sock !== ws) return
        handlers.onStatus('closed')
        // Cap 3s: daemon usually returns fast and the UI is unusable meanwhile.
        const delay = Math.min(3_000, 250 * 2 ** attempt) + Math.random() * 150
        attempt += 1
        timer = setTimeout(connect, delay)
      }
    }

    // 回到分頁／網路恢復時不等 backoff。
    const retryNow = () => {
      if (closed) return
      if (sock && (sock.readyState === WebSocket.OPEN || sock.readyState === WebSocket.CONNECTING)) return
      if (timer) {
        clearTimeout(timer)
        timer = null
      }
      attempt = 0
      connect()
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') retryNow()
    }
    window.addEventListener('online', retryNow)
    window.addEventListener('focus', retryNow)
    document.addEventListener('visibilitychange', onVisible)

    connect()
    return () => {
      closed = true
      window.removeEventListener('online', retryNow)
      window.removeEventListener('focus', retryNow)
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
      sock?.close()
    }
  }
}
