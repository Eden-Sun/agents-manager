import { ApiError } from './types'
import type { ApiErrorBody } from './types'
import { CHECK_MS, CONNECT_TIMEOUT_MS, RESUME_STALE_MS, SILENCE_MS, STABLE_MS, TOKEN_REFRESH_AFTER_FAILS, TOKEN_REFRESH_MIN_GAP_MS, isForeground, realLivenessDeps } from './socketLiveness'
import type { LivenessDeps } from './socketLiveness'

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

/**
 * 非 JSON 的錯誤本文（代理的 HTML 錯誤頁等）：只有短的純文字才當成原因；整頁 HTML 留空，
 * 讓 `ApiError` 退回帶狀態碼的 fallback（否則整頁會變成通知上的提示）。
 */
function plainReason(parsed: unknown): string {
  if (typeof parsed !== 'string') return ''
  const t = parsed.trim()
  return t.length <= 200 && !t.includes('<') ? t : ''
}

export function abortError(): DOMException {
  return new DOMException('上傳已取消', 'AbortError')
}

export function isAbortError(e: unknown): boolean {
  return typeof e === 'object' && e !== null && (e as { name?: unknown }).name === 'AbortError'
}

/**
 * 等 `p`，但 `signal` 中止就立刻以中止原因 reject，不等 `p`。`p` 之後的結果照樣消化（不留未處理的 rejection）。
 * 給共享的工作用：呼叫端只放掉自己的等待，不去中止別人還在等的那一趟。
 */
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

/** 計時器可注入（測試用假的、不靠牆鐘）。 */
export interface TransportTimers {
  setTimeout: (fn: () => void, ms: number) => unknown
  clearTimeout: (id: unknown) => void
}

export interface TransportOptions {
  /** GET 的逾時（毫秒）。預設 30 秒。 */
  getTimeoutMs?: number
  timers?: TransportTimers
}

/** 純文字 token：沒有空白、沒有 HTML 的尖括號，長度有上限。 */
const TOKEN_TEXT = /^[^\s<>]{1,256}$/

/**
 * GET 超過這麼久沒回就中止：掛住的 GET 會讓 `singleFlight` 的 `refreshState` 永遠卡在那一趟。
 * token 更新（`GET /api/session`）用同一個期限：它是共享的，掛住時要能收掉，`refreshing` 才會重建。
 */
const GET_TIMEOUT_MS = 30_000
const realTimers: TransportTimers = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (id) => clearTimeout(id as ReturnType<typeof setTimeout>),
}

export class HttpTransport implements Transport {
  readonly mock = false
  private token = ''
  // 參數屬性在 `erasableSyntaxOnly` 下不能用，所以明寫欄位。
  private readonly opts: TransportOptions
  constructor(opts: TransportOptions = {}) {
    this.opts = opts
  }
  /** 同時只跑一次 `GET /api/session`：401 常常一次來一整批（終端每秒輪詢＋WS 重連）。 */
  private refreshing: Promise<string> | null = null

  /**
   * token 只在開頁時拿一次、存在記憶體裡：那一次沒拿到（daemon 正在重啟／換版），這一頁之後每個請求都
   * 401，畫面就卡在「讀取終端失敗：missing or bad X-AM-Token」直到使用者自己重新整理
   * （2026-09-20 使用者在 blocked 面板上看到）。所以 401 時重拿一次 token 再重試一次。
   */
  private async refreshToken(signal?: AbortSignal): Promise<string> {
    if (!this.refreshing) {
      this.refreshing = this.session().finally(() => {
        this.refreshing = null
      })
    }
    // 共享的這一趟不隨任一呼叫端中止；呼叫端的期限只管它自己的等待。
    return untilAborted(this.refreshing, signal)
  }

  /** 重試過還是 401 就照實丟出去（token 真的不對，重試再多次也一樣）。 */
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
    // 期限包住整趟（headers 與 body）：fetch 不理 signal、body 掛住也一樣，`refreshing` 才收得掉、之後能重建。
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
    // 代理或前端的退回頁會對任何路徑回 200 的 HTML：那不是 token，不能拿去帶給所有請求。
    if ((res.headers?.get?.('content-type') ?? '').includes('text/html')) {
      throw new Error('GET /api/session 回的是網頁，不是 token（代理或前端服務擋住了 /api）')
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
    // GET 沒有呼叫端給的 signal 時加上逾時（POST 不加：逾時的寫入可能已經送達，那是「送達狀態未知」的事）。
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
      // 期限要罩住整段（401 後等 token 更新、重送、讀 body）：只中止第一個 fetch，掛住的 token 更新會讓這趟永遠等。
      const exchange = async () => {
        // 只有 GET 敢重試：POST/PATCH 401 時 daemon 根本沒跑到處理函式，但重送一次仍可能重複送出。
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
      if (timedOut) throw new Error(`${method} ${path} 逾時：${(this.opts.getTimeoutMs ?? GET_TIMEOUT_MS) / 1000} 秒沒有回應`)
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
    // 同 `request` 的 GET：掛住不回就要有期限（issue #1731）。否則 promise 永遠 pending，呼叫端還會把它快取住，之後每次都是同一個轉圈。
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
        // daemon 講的原因在 body 裡（`{error, reason, …}`）。以前只拿 `res.statusText`，畫面上就只剩
        // 「Not Found」／「Conflict」（HTTP/2 連 statusText 都是空字串），連 `file_too_large` 帶的
        // size／max 也一起丟掉（issue #547）。
        const parsed = parseText(res.status, await untilAborted(res.text().catch(() => ''), ctl.signal))
        const body: ApiErrorBody =
          parsed && typeof parsed === 'object'
            ? (parsed as ApiErrorBody)
            : { reason: plainReason(parsed) || res.statusText }
        throw new ApiError(res.status, body, `GET ${path} failed (${res.status})`)
      }
      return URL.createObjectURL(await untilAborted(res.blob(), ctl.signal))
    } catch (e) {
      if (timedOut) throw new Error(`GET ${path} 逾時：${limitMs / 1000} 秒沒有回應`)
      throw e
    } finally {
      timers.clearTimeout(timeoutId)
    }
  }

  openSocket(handlers: SocketHandlers, liveness: LivenessDeps = realLivenessDeps): () => void {
    let closed = false
    /** 最後一次收到任何幀（含心跳 ping）或連上的時刻；見 `socketLiveness.ts`（issue #760）。 */
    let lastActive = liveness.now()
    let connectingSince = liveness.now()
    /**
     * 這個 daemon 會送心跳（這個分頁見過 `ping`）。舊 daemon 不送，安靜時整條線一個幀都沒有，靜默不是斷線的證據；
     * 沒有這個旗標的話，舊 daemon 搭新前端（vite dev 連 7788、daemon 回滾）閒置每分鐘就判半開重連，
     * 而每次重連都整份重抓 state、已載入的對話、額度與草稿。旗標跨重連保留，所以新連線在第一個 ping 之前也有死線偵測。
     */
    let daemonPings = false
    let attempt = 0
    /** 目前這條連線是什麼時候開成的（沒開成＝null）；開了不到 `STABLE_MS` 就被關不算穩定，退避不歸零。 */
    let openedAt: number | null = null
    /** 連續「根本沒開成」的次數：握手被拒（token 不對）瀏覽器只看得到沒開成，沒有 HTTP 狀態可看。 */
    let failsBeforeOpen = 0
    let lastTokenRefresh = Number.NEGATIVE_INFINITY
    let sock: WebSocket | null = null
    let timer: ReturnType<typeof setTimeout> | null = null

    const connect = () => {
      if (closed) return
      connectingSince = liveness.now()
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
        lastActive = liveness.now()
        // 不在這裡把 attempt 歸零：開成功又馬上被關的連線（代理、半壞的 daemon）會讓退避永遠停在 250ms，而每次「開成功」
        // 都整份重抓 state／對話／額度／草稿。歸零留到 onclose 確認這條線活夠久。
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
          /* malformed */
        }
        if (!frame || typeof frame.type !== 'string') return
        // daemon 的心跳只是存活證據，不是事件。
        if (frame.type === 'ping') {
          daemonPings = true
          return
        }
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
        if (openedAt === null) failsBeforeOpen += 1
        else if (liveness.now() - openedAt >= STABLE_MS) attempt = 0
        openedAt = null
        // Cap 3s: daemon usually returns fast and the UI is unusable meanwhile.
        // 一半固定、一半隨機（equal jitter）：daemon 重啟時一群分頁同時重連，只加 150ms 的話全部落在同一刻。
        const base = Math.min(3_000, 250 * 2 ** attempt)
        const delay = base / 2 + Math.random() * (base / 2)
        attempt += 1
        timer = setTimeout(() => {
          timer = null
          // 握手一直被拒：多半是 daemon 換了 token（HTTP 的 GET 撞 401 會自己重拿，ws 沒有狀態碼可看，所以在這裡補）。
          // 先失敗幾次再拿（daemon 只是還沒起來時 /api/session 也不通），而且兩次之間隔開。
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
    // 切回前景：`readyState` 是 OPEN 也不一定活著（半開），靜默超過心跳間隔就當死線，丟掉重連（`since` 補洞、open handler 重抓 state）。
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
    // 前景裡靜默太久：同上。背景分頁不查——timer 被節流、frame 也可能被凍住，靜默不代表斷線，回前景那一刻再判斷。
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
