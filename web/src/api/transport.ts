/**
 * Transport abstraction: one implementation talks to the real daemon over
 * fetch + WebSocket, the other (`mock.ts`) serves everything from memory.
 * `index.ts` picks between them; nothing else in the app knows which is live.
 */

import { ApiError } from './types'
import type { ApiErrorBody } from './types'

export type HttpMethod = 'GET' | 'POST' | 'PATCH' | 'DELETE'

export interface SocketHandlers {
  /** A `{seq, type, data}` frame from `/ws`. */
  onFrame: (frame: { seq?: number; type: string; data?: unknown }) => void
  /** Socket liveness, for the header indicator. */
  onStatus: (status: 'connecting' | 'open' | 'closed') => void
  /** Highest seq the client has processed; sent as `?since=`. */
  since: () => number
}

export interface Transport {
  readonly mock: boolean
  /** `GET /api/session` → token, cached for subsequent calls. */
  session: () => Promise<string>
  request: (method: HttpMethod, path: string, body?: unknown) => Promise<unknown>
  /** POST raw bytes (an image) with `Content-Type: <mime>`; used by attachment upload. */
  upload: (path: string, file: Blob) => Promise<unknown>
  /**
   * GET binary content as an object URL. Attachments are behind the token, and an
   * `<img src>` cannot carry a header — so the bytes are fetched and blob-URL'd instead.
   */
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
      // 先把上一條 socket 徹底斷乾淨再開新的。CLOSING 中的 socket 過得了 `retryNow` 的檢查，
      // 它的 onclose 之後才會補跑，那時 `closed` 還是 false——狀態會被它改成 closed、還會再排
      // 一次 connect，最後變成兩條都活著、每個 frame 收兩次。
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
        try {
          const frame = JSON.parse(ev.data) as { seq?: number; type: string; data?: unknown }
          if (frame && typeof frame.type === 'string') handlers.onFrame(frame)
        } catch {
          /* ignore malformed frame */
        }
      }
      ws.onerror = () => ws.close()
      ws.onclose = () => {
        // 已經被新的連線取代掉的話什麼都別做（狀態和重連都歸新的那條管）。
        if (closed || sock !== ws) return
        handlers.onStatus('closed')
        // Exponential backoff, capped at 3s — a dropped daemon usually comes back fast and
        // the UI is unusable until it does, so waiting 10s for a retry is worse than the
        // extra attempts.
        const delay = Math.min(3_000, 250 * 2 ** attempt) + Math.random() * 150
        attempt += 1
        timer = setTimeout(connect, delay)
      }
    }

    // 回到分頁 / 網路回來時不要等 backoff 跑完，直接重試一次。
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
