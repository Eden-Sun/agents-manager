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
  request: (method: HttpMethod, path: string, body?: unknown) => Promise<unknown>
  upload: (path: string, file: Blob) => Promise<unknown>
  /** Attachments need the token header, which `<img src>` can't carry — so fetch and blob-URL. */
  blobUrl: (path: string) => Promise<string>
  openSocket: (handlers: SocketHandlers) => () => void
}

async function readBody(res: Response): Promise<unknown> {
  if (res.status === 204) return null
  const text = await res.text()
  if (!text) return null
  try {
    return JSON.parse(text) as unknown
  } catch {
    return text
  }
}

export class HttpTransport implements Transport {
  readonly mock = false
  private token = ''

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

  async request(method: HttpMethod, path: string, body?: unknown): Promise<unknown> {
    const headers: Record<string, string> = { Accept: 'application/json' }
    if (this.token) headers['X-AM-Token'] = this.token
    if (body !== undefined) headers['Content-Type'] = 'application/json'
    const res = await fetch(`/api${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    })
    const parsed = await readBody(res)
    if (!res.ok) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: String(parsed ?? '') }
      throw new ApiError(res.status, errBody, `${method} ${path} failed (${res.status})`)
    }
    return parsed
  }

  async upload(path: string, file: Blob): Promise<unknown> {
    const headers: Record<string, string> = { Accept: 'application/json' }
    if (this.token) headers['X-AM-Token'] = this.token
    headers['Content-Type'] = file.type || 'application/octet-stream'
    const res = await fetch(`/api${path}`, { method: 'POST', headers, body: file })
    const parsed = await readBody(res)
    if (!res.ok) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: String(parsed ?? '') }
      throw new ApiError(res.status, errBody, `POST ${path} failed (${res.status})`)
    }
    return parsed
  }

  async blobUrl(path: string): Promise<string> {
    const headers: Record<string, string> = {}
    if (this.token) headers['X-AM-Token'] = this.token
    const res = await fetch(`/api${path}`, { headers })
    if (!res.ok) throw new ApiError(res.status, { reason: res.statusText }, `GET ${path} failed (${res.status})`)
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
