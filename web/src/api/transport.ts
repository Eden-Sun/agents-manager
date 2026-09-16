import { isPairingRequired, normalizePairCode } from '../lib/pairing'
import { deviceTokenStore } from './sessionToken'
import type { TokenStore } from './sessionToken'
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
  /** 配對換來的 token：記在 transport 上，並存進這台裝置（SPEC §7.1a）。 */
  adoptToken: (token: string) => void
  /**
   * token 被打回 401、重新取得時又被要求配對，就回呼一次——App 據此回到配對畫面。
   * 傳 `null` 取消登記。
   */
  setPairingListener: (cb: (() => void) | null) => void
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

function tokenOf(body: unknown): string {
  if (typeof body === 'string') return body
  if (body && typeof body === 'object' && typeof (body as { token?: unknown }).token === 'string') {
    return (body as { token: string }).token
  }
  return ''
}

export class HttpTransport implements Transport {
  readonly mock = false
  private token = ''
  private pairingListener: (() => void) | null = null
  /** 正在重新取得 token 的那一次。並行請求同時吃到 401 時共用它，不會打 N 次 `/api/session`。 */
  private reacquiring: Promise<string> | null = null

  private readonly tokens: TokenStore

  constructor(tokens: TokenStore = deviceTokenStore()) {
    this.tokens = tokens
    this.token = tokens.get()
  }

  adoptToken(token: string): void {
    this.token = token
    this.tokens.set(token)
  }

  setPairingListener(cb: (() => void) | null): void {
    this.pairingListener = cb
  }

  /**
   * 這台裝置存過 token 就直接用，連 `/api/session` 都不問：非 loopback 問了只會拿到
   * 403 `pairing_required`（SPEC §7.1a），配過一次的手機不該因為重整又被擋在門外。
   */
  session(): Promise<string> {
    const saved = this.tokens.get()
    if (saved) {
      this.token = saved
      return Promise.resolve(saved)
    }
    return this.fetchSessionToken()
  }

  private async fetchSessionToken(): Promise<string> {
    const res = await fetch('/api/session', { headers: { Accept: 'application/json' } })
    const body = await readBody(res)
    if (!res.ok) {
      throw new ApiError(res.status, (body ?? {}) as ApiErrorBody, `GET /api/session failed (${res.status})`)
    }
    const token = tokenOf(body)
    if (!token) throw new Error('GET /api/session returned no token')
    this.adoptToken(token)
    return token
  }

  /**
   * 401：手上這把 token 已經不算數（daemon 換過、或使用者清過）。丟掉它、重新取得一把。
   * 拿不到（非 loopback 又要配對）就回空字串，呼叫端照原本的 401 收場——**不再往下繞**。
   */
  private reacquireToken(): Promise<string> {
    if (this.reacquiring) return this.reacquiring
    this.token = ''
    this.tokens.clear()
    this.reacquiring = this.fetchSessionToken()
      .catch((e: unknown) => {
        if (isPairingRequired(e)) this.pairingListener?.()
        return ''
      })
      .finally(() => {
        this.reacquiring = null
      })
    return this.reacquiring
  }

  /**
   * 帶 token 送一次；401 就重新取得再送一次。**只重試一次**——第二次還是 401 就照實吐出去，
   * 不然 token 一直錯就會變成打不完的迴圈。
   */
  private async authed(run: (token: string) => Promise<Response>): Promise<Response> {
    const res = await run(this.token)
    if (res.status !== 401) return res
    const fresh = await this.reacquireToken()
    if (!fresh) return res
    return run(fresh)
  }

  async request(method: HttpMethod, path: string, body?: unknown): Promise<unknown> {
    const res = await this.authed((token) => {
      const headers: Record<string, string> = { Accept: 'application/json' }
      if (token) headers['X-AM-Token'] = token
      if (body !== undefined) headers['Content-Type'] = 'application/json'
      return fetch(`/api${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
      })
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
    const res = await this.authed((token) => {
      const headers: Record<string, string> = { Accept: 'application/json' }
      if (token) headers['X-AM-Token'] = token
      headers['Content-Type'] = file.type || 'application/octet-stream'
      return fetch(`/api${path}`, { method: 'POST', headers, body: file })
    })
    const parsed = await readBody(res)
    if (!res.ok) {
      const errBody: ApiErrorBody =
        parsed && typeof parsed === 'object' ? (parsed as ApiErrorBody) : { reason: String(parsed ?? '') }
      throw new ApiError(res.status, errBody, `POST ${path} failed (${res.status})`)
    }
    return parsed
  }

  async blobUrl(path: string): Promise<string> {
    const res = await this.authed((token) => {
      const headers: Record<string, string> = {}
      if (token) headers['X-AM-Token'] = token
      return fetch(`/api${path}`, { headers })
    })
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

/**
 * 用一次性配對碼換 token 並存進這台裝置（`POST /api/session/pair`，不需 token）。
 * 真假 transport 共用這一條——它只是 `request` ＋ `adoptToken`，沒有第二套規矩。
 * 失敗照原樣丟 `ApiError`：403 `pairing_failed`／429 `pairing_rate_limited`。
 */
export async function pairWithCode(t: Pick<Transport, 'request' | 'adoptToken'>, code: string): Promise<void> {
  const raw = await t.request('POST', '/session/pair', { code: normalizePairCode(code) })
  const token = tokenOf(raw)
  if (!token) throw new Error('POST /api/session/pair returned no token')
  t.adoptToken(token)
}
