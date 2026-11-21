import { ShareHttpError, shareBeforeCursor, toShareFiles, toShareMessage, toSharePage, toStatus, type ShareFile, type ShareMessage, type SharePage, type ShareStatus } from './shareModel'
import { t } from './shareI18n'

export class ShareTimeoutError extends Error {
  constructor() {
    super(t('分享頁請求逾時：伺服器沒有在期限內回應'))
  }
}

export interface ShareEvents {
  onMessage: (m: ShareMessage) => void
  onStatus: (s: ShareStatus) => void
  onDown: () => void
  onResync?: () => void
  onUp?: () => void
}

export interface ShareSendResult {
  delivery?: string
  messageId?: string
}

export interface ShareClient {
  messages(before?: string): Promise<SharePage>
  send(text: string, clientRequestId: string, attachments: string[]): Promise<ShareSendResult | void>
  upload(file: File, signal?: AbortSignal): Promise<{ id: string; name: string }>
  files(): Promise<ShareFile[]>
  fileUrl(name: string): string
  previewUrl(name: string, version?: string): string
  fileBlob(name: string): Promise<Blob>
  subscribe(ev: ShareEvents): () => void
}

async function check(res: Response): Promise<Response> {
  if (res.ok) return res
  const ra = Number(res.headers.get('Retry-After'))
  let reason: string | null = null
  if (res.status === 400) {
    try {
      const b = (await res.json()) as { reason?: unknown }
      if (typeof b?.reason === 'string') reason = b.reason
    } catch {
    }
  }
  throw new ShareHttpError(res.status, Number.isFinite(ra) && ra > 0 ? ra : null, reason)
}

const BLOB_RETRY_MAX = 6
const BLOB_WAIT_MAX_S = 10

const HEADERS_DEADLINE_MS = 30_000
const BODY_IDLE_MS = 30_000

export interface ShareClientOptions {
  sleep?: (ms: number) => Promise<void>
  timers?: { setTimeout: (fn: () => void, ms: number) => unknown; clearTimeout: (id: unknown) => void }
}

function fetchWithDeadline<T>(
  timers: NonNullable<ShareClientOptions['timers']>,
  url: string,
  init: RequestInit,
  read: (res: Response, touch: () => void) => Promise<T>,
): Promise<T> {
  const ctl = new AbortController()
  const expired = new Promise<never>((_, reject) => {
    ctl.signal.addEventListener('abort', () => reject(new ShareTimeoutError()))
  })
  expired.catch(() => {})
  let headers: unknown = timers.setTimeout(() => ctl.abort(), HEADERS_DEADLINE_MS)
  let idle: unknown = null
  let done = false
  const touch = () => {
    if (done) return
    if (idle !== null) timers.clearTimeout(idle)
    idle = timers.setTimeout(() => ctl.abort(), BODY_IDLE_MS)
  }
  const run = async () => {
    const res = await fetch(url, { ...init, signal: ctl.signal })
    timers.clearTimeout(headers)
    headers = null
    touch()
    return read(res, touch)
  }
  return Promise.race([run(), expired]).finally(() => {
    done = true
    if (headers !== null) timers.clearTimeout(headers)
    if (idle !== null) timers.clearTimeout(idle)
  })
}

async function readBody(res: Response, touch: () => void): Promise<Uint8Array<ArrayBuffer>> {
  if (!res.body) return new Uint8Array(await res.arrayBuffer())
  const reader = res.body.getReader()
  const parts: Uint8Array[] = []
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    touch()
    parts.push(value)
  }
  const out = new Uint8Array(new ArrayBuffer(parts.reduce((n, p) => n + p.byteLength, 0)))
  let off = 0
  for (const p of parts) {
    out.set(p, off)
    off += p.byteLength
  }
  return out
}

export function httpShareClient(token: string, opts: ShareClientOptions = {}): ShareClient {
  const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)))
  const timers = opts.timers ?? { setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms), clearTimeout: (id: unknown) => clearTimeout(id as ReturnType<typeof setTimeout>) }
  const base = `/s/${encodeURIComponent(token)}/api`
  const init: RequestInit = { credentials: 'omit', referrerPolicy: 'no-referrer', cache: 'no-store' }
  const json = async (path: string, extra: RequestInit = {}) => (await check(await fetch(`${base}${path}`, { ...init, ...extra }))).json() as Promise<unknown>
  const getJson = (path: string) =>
    fetchWithDeadline(timers, `${base}${path}`, init, async (res, touch) => {
      await check(res)
      return JSON.parse(new TextDecoder().decode(await readBody(res, touch))) as unknown
    })
  const fetchBlob = async (name: string): Promise<Blob> => {
    for (let attempt = 0; ; attempt++) {
      try {
        return await fetchWithDeadline(timers, `${base}/files/${encodeURIComponent(name)}`, init, async (res, touch) => {
          await check(res)
          return new Blob([await readBody(res, touch)], { type: res.headers.get('Content-Type') ?? '' })
        })
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
          }
        })
        sock.addEventListener('status', (e) => {
          try {
            ev.onStatus(toStatus(JSON.parse((e as MessageEvent<string>).data)))
          } catch {
          }
        })
        sock.addEventListener('resync', () => ev.onResync?.())
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
