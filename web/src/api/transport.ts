import { ApiError } from './types'
import type { ApiErrorBody } from './types'
import { CHECK_MS, CONNECT_TIMEOUT_MS, RESUME_STALE_MS, SILENCE_MS, STABLE_MS, TOKEN_REFRESH_AFTER_FAILS, TOKEN_REFRESH_MIN_GAP_MS, isForeground, realLivenessDeps } from './socketLiveness'
import type { LivenessDeps } from './socketLiveness'
import { t } from '../lib/i18n'

export type HttpMethod = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'

export interface SocketHandlers {
  onFrame: (frame: { seq?: number; type: string; data?: unknown }) => void
  onStatus: (status: 'connecting' | 'open' | 'closed') => void
  since: () => number
}

export interface Transport {
  readonly mock: boolean
  session: () => Promise<string>
  request: (method: HttpMethod, path: string, body?: unknown, signal?: AbortSignal) => Promise<unknown>
  upload: (path: string, file: Blob, opts?: UploadOptions) => Promise<unknown>
  blobUrl: (path: string) => Promise<string>
  openSocket: (handlers: SocketHandlers) => () => void
  openStream?: (path: string, handlers: StreamHandlers) => () => void
}

export interface StreamHandlers {
  onMessage: (msg: unknown) => void
  onOpen?: (send: (msg: unknown) => void) => void
  onClose: (opened: boolean) => void
}

export interface UploadOptions {
  signal?: AbortSignal
  onProgress?: (loaded: number, total: number) => void
}

function plainReason(parsed: unknown): string {
  if (typeof parsed !== 'string') return ''
  const t = parsed.trim()
  return t.length <= 200 && !t.includes('<') ? t : ''
}

export function abortError(): DOMException {
  return new DOMException(t('上傳已取消'), 'AbortError')
}

export function isAbortError(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { name?: unknown }).name === 'AbortError'
}

function untilAborted<T>(p: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (!signal) return p
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason ?? abortError())
    p.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort))
    if (signal.aborted) onAbort()
    else signal.addEventListener('abort', onAbort, { once: true })
  })
}

function parseText(status: number, text: string): unknown {
  if (status === 204 || !text) return null
  try {
    return JSON.parse(text) as unknown
  } catch {
    return text
  }
}

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
      reject(new Error(t('連線中斷，上傳沒有完成')))
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

export interface TransportTimers {
  setTimeout: (fn: () => void, ms: number) => unknown
  clearTimeout: (id: unknown) => void
}

export interface TransportOptions {
  getTimeoutMs?: number
  timers?: TransportTimers
}

const TOKEN_TEXT = /^[^\s<>]{1,256}$/

const GET_TIMEOUT_MS = 30_000
const realTimers: TransportTimers = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (id) => clearTimeout(id as ReturnType<typeof setTimeout>),
}

export class HttpTransport implements Transport {
  readonly mock = false
  private token = ''
  private readonly opts: TransportOptions
  constructor(opts: TransportOptions = {}) {
    this.opts = opts
  }
  private refreshing: Promise<string> | null = null

  private async refreshToken(signal?: AbortSignal): Promise<string> {
    if (!this.refreshing) {
      this.refreshing = this.session().finally(() => {
        this.refreshing = null
      })
    }
    return untilAborted(this.refreshing, signal)
  }

  private async withFreshToken<T>(
    send: (tok: string) => Promise<Response>,
    run: (res: Response) => Promise<T>,
    signal?: AbortSignal,
  ): Promise<T> {
    let res = await send(this.token)
    if (res.status === 401) {
      const before = this.token
      let tok = ''
      try {
        tok = await this.refreshToken(signal)
      } catch {
        return run(res)
      }
      if (tok && tok !== before) res = await send(tok)
    }
    return run(res)
  }

  async session(): Promise<string> {
    const timers = this.opts.timers ?? realTimers
    const ctl = new AbortController()
    const timeoutId = timers.setTimeout(() => ctl.abort(), GET_TIMEOUT_MS)
    try {
      return await untilAborted(this.fetchSession(ctl.signal), ctl.signal)
    } finally {
      timers.clearTimeout(timeoutId)
    }
  }

  private async fetchSession(signal: AbortSignal): Promise<string> {
    const res = await fetch('/api/session', { headers: { Accept: 'application/json' }, signal })
    const body = await readBody(res)
    if (!res.ok) {
      throw new ApiError(res.status, (body ?? {}) as ApiErrorBody, `GET /api/session failed (${res.status})`)
    }
    if ((res.headers?.get?.('content-type') ?? '').includes('text/html')) {
      throw new Error(t('GET /api/session 回的是網頁，不是 token（代理或前端服務擋住了 /api）'))
    }
    const token =
      typeof body === 'string'
        ? TOKEN_TEXT.test(body)
          ? body
          : ''
        : body && typeof body === 'object' && typeof (body as { token?: unknown }).token === 'string'
          ? (body as { token: string }).token
          : ''
    if (!token) throw new Error('GET /api/session returned no token')
    this.token = token
    return token
  }

  async request(method: HttpMethod, path: string, body?: unknown, signal?: AbortSignal): Promise<unknown> {
    const timers = this.opts.timers ?? realTimers
    let timedOut = false
    let timeoutId: unknown = null
    let effective = signal
    if (!signal && method === 'GET') {
      const ctl = new AbortController()
      timeoutId = timers.setTimeout(() => {
        timedOut = true
        ctl.abort()
      }, this.opts.getTimeoutMs ?? GET_TIMEOUT_MS)
      effective = ctl.signal
    }
    try {
      const send = (tok: string) => {
        const headers: Record<string, string> = { Accept: 'application/json' }
        if (tok) headers['X-AM-Token'] = tok
        if (body !== undefined) headers['Content-Type'] = 'application/json'
        return fetch(`/api${path}`, {
          method,
          headers,
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: effective,
        })
      }
      const exchange = async () => {
        const res = method === 'GET' ? await this.withFreshToken(send, async (r) => r, effective) : await send(this.token)
        return { res, parsed: await readBody(res) }
      }
      const { res, parsed } = await untilAborted(exchange(), effective)
      if (!res.ok) {
        const errBody: ApiErrorBody =
          parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: plainReason(parsed) }
        throw new ApiError(res.status, errBody, `${method} ${path} failed (${res.status})`)
      }
      return parsed
    } catch (e) {
      if (timedOut) throw new Error(t('{method} {path} 逾時：{sec} 秒沒有回應', { method, path, sec: (this.opts.getTimeoutMs ?? GET_TIMEOUT_MS) / 1000 }))
      throw e
    } finally {
      if (timeoutId !== null) timers.clearTimeout(timeoutId)
    }
  }

  async upload(path: string, file: Blob, opts?: UploadOptions): Promise<unknown> {
    const { status, body: parsed } = await xhrUpload(`/api${path}`, file, this.token, opts)
    if (status < 200 || status >= 300) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: plainReason(parsed) }
      throw new ApiError(status, errBody, `POST ${path} failed (${status})`)
    }
    return parsed
  }

  async blobUrl(path: string): Promise<string> {
    const timers = this.opts.timers ?? realTimers
    const limitMs = this.opts.getTimeoutMs ?? GET_TIMEOUT_MS
    const ctl = new AbortController()
    let timedOut = false
    const timeoutId = timers.setTimeout(() => {
      timedOut = true
      ctl.abort()
    }, limitMs)
    try {
      const send = (tok: string) => fetch(`/api${path}`, { headers: tok ? { 'X-AM-Token': tok } : {}, signal: ctl.signal })
      const res = await untilAborted(this.withFreshToken(send, async (r) => r, ctl.signal), ctl.signal)
      if (!res.ok) {
        const parsed = parseText(res.status, await untilAborted(res.text().catch(() => ''), ctl.signal))
        const body: ApiErrorBody =
          parsed && typeof parsed === 'object'
            ? (parsed as ApiErrorBody)
            : { reason: plainReason(parsed) || res.statusText }
        throw new ApiError(res.status, body, `GET ${path} failed (${res.status})`)
      }
      return URL.createObjectURL(await untilAborted(res.blob(), ctl.signal))
    } catch (e) {
      if (timedOut) throw new Error(t('GET {path} 逾時：{sec} 秒沒有回應', { path, sec: limitMs / 1000 }))
      throw e
    } finally {
      timers.clearTimeout(timeoutId)
    }
  }

  openStream(path: string, handlers: StreamHandlers): () => void {
    let closed = false
    let opened = false
    let sock: WebSocket | null = null
    const connect = () => {
      if (closed) return
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
      const qs = new URLSearchParams({ token: this.token })
      const ws = new WebSocket(`${scheme}://${location.host}/api${path}${path.includes('?') ? '&' : '?'}${qs.toString()}`)
      sock = ws
      ws.onopen = () => {
        if (sock !== ws) return
        opened = true
        handlers.onOpen?.((msg) => {
          if (sock === ws && ws.readyState === 1) ws.send(JSON.stringify(msg))
        })
      }
      ws.onmessage = (ev: MessageEvent<string>) => {
        if (sock !== ws) return
        let msg: unknown = null
        try {
          msg = JSON.parse(ev.data)
        } catch {
          return
        }
        handlers.onMessage(msg)
      }
      ws.onerror = () => {}
      ws.onclose = () => {
        if (sock !== ws || closed) return
        sock = null
        handlers.onClose(opened)
      }
    }
    if (this.token) connect()
    else
      void this.refreshToken().then(connect, () => {
        if (!closed) handlers.onClose(false)
      })
    return () => {
      closed = true
      const old = sock
      sock = null
      if (old) old.close()
    }
  }

  openSocket(handlers: SocketHandlers, liveness: LivenessDeps = realLivenessDeps): () => void {
    let closed = false
    let lastActive = liveness.now()
    let connectingSince = liveness.now()
    let daemonPings = false
    let attempt = 0
    let openedAt: number | null = null
    let failsBeforeOpen = 0
    let lastTokenRefresh = Number.NEGATIVE_INFINITY
    let sock: WebSocket | null = null
    let timer: ReturnType<typeof setTimeout> | null = null

    const connect = () => {
      if (closed) return
      connectingSince = liveness.now()
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
      if (!this.token) {
        void this.refreshToken().then(retryNow, () => {})
      }
      const qs = new URLSearchParams({ token: this.token, since: String(handlers.since()) })
      const ws = new WebSocket(`${scheme}://${location.host}/ws?${qs.toString()}`)
      sock = ws
      ws.onopen = () => {
        if (sock !== ws) return
        lastActive = liveness.now()
        openedAt = liveness.now()
        failsBeforeOpen = 0
        handlers.onStatus('open')
      }
      ws.onmessage = (ev: MessageEvent<string>) => {
        if (sock !== ws) return
        lastActive = liveness.now()
        let frame: { seq?: number; type: string; data?: unknown } | null = null
        try {
          frame = JSON.parse(ev.data) as { seq?: number; type: string; data?: unknown }
        } catch {
        }
        if (!frame || typeof frame.type !== 'string') return
        if (frame.type === 'ping') {
          daemonPings = true
          return
        }
        try {
          handlers.onFrame(frame)
        } catch (e) {
          console.error(`[ws] handler threw on ${frame.type} frame`, e)
        }
      }
      ws.onerror = () => ws.close()
      ws.onclose = () => {
        if (closed || sock !== ws) return
        handlers.onStatus('closed')
        if (openedAt === null) failsBeforeOpen += 1
        else if (liveness.now() - openedAt >= STABLE_MS) attempt = 0
        openedAt = null
        const base = Math.min(3_000, 250 * 2 ** attempt)
        const delay = base / 2 + Math.random() * (base / 2)
        attempt += 1
        timer = setTimeout(() => {
          timer = null
          if (failsBeforeOpen >= TOKEN_REFRESH_AFTER_FAILS && liveness.now() - lastTokenRefresh >= TOKEN_REFRESH_MIN_GAP_MS) {
            lastTokenRefresh = liveness.now()
            void this.refreshToken().then(
              () => {},
              () => {},
            ).finally(connect)
            return
          }
          connect()
        }, delay)
      }
    }

    const stuckConnecting = () =>
      sock !== null && sock.readyState === WebSocket.CONNECTING && liveness.now() - connectingSince > CONNECT_TIMEOUT_MS

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
    const resume = () => {
      if (closed || !isForeground()) return
      if (stuckConnecting()) {
        failsBeforeOpen += 1
        connect()
        return
      }
      if (daemonPings && sock && sock.readyState === WebSocket.OPEN && liveness.now() - lastActive > RESUME_STALE_MS) {
        connect()
        return
      }
      retryNow()
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') resume()
    }
    const stopWatchdog = liveness.every(() => {
      if (closed || !isForeground()) return
      if (stuckConnecting()) {
        failsBeforeOpen += 1
        connect()
        return
      }
      if (daemonPings && sock && sock.readyState === WebSocket.OPEN && liveness.now() - lastActive > SILENCE_MS) connect()
    }, CHECK_MS)
    window.addEventListener('online', resume)
    window.addEventListener('focus', resume)
    document.addEventListener('visibilitychange', onVisible)

    connect()
    return () => {
      closed = true
      stopWatchdog()
      window.removeEventListener('online', resume)
      window.removeEventListener('focus', resume)
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
      sock?.close()
    }
  }
}
