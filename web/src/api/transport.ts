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

  openSocket(handlers: SocketHandlers): () => void {
    let closed = false
    let attempt = 0
    let sock: WebSocket | null = null
    let timer: ReturnType<typeof setTimeout> | null = null

    const connect = () => {
      if (closed) return
      handlers.onStatus('connecting')
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
      const qs = new URLSearchParams({ token: this.token, since: String(handlers.since()) })
      const ws = new WebSocket(`${scheme}://${location.host}/ws?${qs.toString()}`)
      sock = ws
      ws.onopen = () => {
        attempt = 0
        handlers.onStatus('open')
      }
      ws.onmessage = (ev: MessageEvent<string>) => {
        try {
          const frame = JSON.parse(ev.data) as { seq?: number; type: string; data?: unknown }
          if (frame && typeof frame.type === 'string') handlers.onFrame(frame)
        } catch {
          /* ignore malformed frame */
        }
      }
      ws.onerror = () => ws.close()
      ws.onclose = () => {
        if (closed) return
        handlers.onStatus('closed')
        // Exponential backoff, capped at 10s.
        const delay = Math.min(10_000, 500 * 2 ** attempt) + Math.random() * 250
        attempt += 1
        timer = setTimeout(connect, delay)
      }
    }

    connect()
    return () => {
      closed = true
      if (timer) clearTimeout(timer)
      sock?.close()
    }
  }
}
