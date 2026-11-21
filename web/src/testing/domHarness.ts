import { GlobalRegistrator } from '@happy-dom/global-registrator'
import { resetTranslateReady } from '../components/useTranslateReady'
import { currentTestScope, endCurrentTest, endedTestScope } from './testScope'
import type { TestScope } from './testScope'
import type { ReactElement } from 'react'

const originalFetch = globalThis.fetch
const nativeTimers = {
  queueMicrotask: globalThis.queueMicrotask,
  setTimeout: globalThis.setTimeout,
  clearTimeout: globalThis.clearTimeout,
  setInterval: globalThis.setInterval,
  clearInterval: globalThis.clearInterval,
}
const monotonicNow = globalThis.performance.now.bind(globalThis.performance)

interface PendingDomTimer {
  kind: 'timeout' | 'interval'
  timeout: number
  dueAt: number
  testName: string
  stack: string
}
const trackedDomTimeouts = new Map<unknown, PendingDomTimer>()
const trackedDomIntervals = new Map<unknown, PendingDomTimer>()
const DOM_TIMER_DRAIN_TIMEOUT_MS = 2000
let domLeaseTail: Promise<void> = Promise.resolve()
let releaseDomLease: (() => void) | null = null

function trackedSetTimeout(handler: TimerHandler, timeout?: number, ...args: unknown[]): ReturnType<typeof setTimeout> {
  const ms = typeof timeout === 'number' && Number.isFinite(timeout) ? Math.max(0, timeout) : 0
  const stack = new Error().stack ?? ''
  let id: ReturnType<typeof setTimeout>
  id = nativeTimers.setTimeout.call(globalThis, () => {
    trackedDomTimeouts.delete(id)
    if (typeof handler === 'function') {
      ;(handler as (...a: unknown[]) => void)(...args)
    } else if (typeof handler === 'string') {
      throw new TypeError('字串形式的 setTimeout handler 不支援於 DOM 測試 harness')
    }
  }, ms)
  trackedDomTimeouts.set(id, {
    kind: 'timeout',
    timeout: ms,
    dueAt: monotonicNow() + ms,
    testName: currentTestScope()?.name ?? '<測試 hook／模組初始化>',
    stack,
  })
  return id
}

function trackedClearTimeout(id?: ReturnType<typeof setTimeout>): void {
  if (id !== undefined) trackedDomTimeouts.delete(id)
  nativeTimers.clearTimeout.call(globalThis, id)
}

function trackedSetInterval(handler: TimerHandler, timeout?: number, ...args: unknown[]): ReturnType<typeof setInterval> {
  const ms = typeof timeout === 'number' && Number.isFinite(timeout) ? Math.max(0, timeout) : 0
  const stack = new Error().stack ?? ''
  let id: ReturnType<typeof setInterval>
  id = nativeTimers.setInterval.call(globalThis, () => {
    if (typeof handler === 'function') {
      ;(handler as (...a: unknown[]) => void)(...args)
    } else if (typeof handler === 'string') {
      throw new TypeError('字串形式的 setInterval handler 不支援於 DOM 測試 harness')
    }
  }, ms)
  trackedDomIntervals.set(id, {
    kind: 'interval',
    timeout: ms,
    dueAt: monotonicNow() + ms,
    testName: currentTestScope()?.name ?? '<測試 hook／模組初始化>',
    stack,
  })
  return id
}

function trackedClearInterval(id?: ReturnType<typeof setInterval>): void {
  if (id !== undefined) trackedDomIntervals.delete(id)
  nativeTimers.clearInterval.call(globalThis, id)
}

function pendingTimers(): PendingDomTimer[] {
  return [...trackedDomTimeouts.values(), ...trackedDomIntervals.values()]
}

function pendingTimerError(reason: string): Error {
  const details = pendingTimers().map((timer) =>
    `- ${timer.kind}（${timer.timeout}ms），測試「${timer.testName}」\n${timer.stack}`,
  )
  return new Error(`${reason}\n未收束的 DOM timer：\n${details.join('\n')}`)
}

function clearPendingDomTimers(): void {
  for (const id of trackedDomTimeouts.keys()) {
    nativeTimers.clearTimeout.call(globalThis, id as ReturnType<typeof setTimeout>)
  }
  trackedDomTimeouts.clear()
  for (const id of trackedDomIntervals.keys()) {
    nativeTimers.clearInterval.call(globalThis, id as ReturnType<typeof setInterval>)
  }
  trackedDomIntervals.clear()
}

async function drainDomTimers(): Promise<void> {
  const deadline = monotonicNow() + DOM_TIMER_DRAIN_TIMEOUT_MS
  while (true) {
    if (trackedDomIntervals.size > 0) {
      throw pendingTimerError('DOM 測試結束時仍有未清除的 interval')
    }
    while ([...trackedDomTimeouts.values()].some((timer) => timer.timeout <= DOM_TIMER_DRAIN_TIMEOUT_MS)) {
      const pending = [...trackedDomTimeouts.values()].filter((timer) => timer.timeout <= DOM_TIMER_DRAIN_TIMEOUT_MS)
      const remaining = deadline - monotonicNow()
      if (remaining <= 0) {
        throw pendingTimerError(`DOM timer 超過 ${DOM_TIMER_DRAIN_TIMEOUT_MS}ms 仍未收束`)
      }
      const nextDueAt = Math.min(...pending.map((timer) => timer.dueAt))
      if (nextDueAt > deadline) {
        throw pendingTimerError(`DOM timer 超過 ${DOM_TIMER_DRAIN_TIMEOUT_MS}ms 仍未到期`)
      }
      const delay = Math.min(Math.max(0, nextDueAt - monotonicNow()), remaining)
      await new Promise<void>((resolve) => nativeTimers.setTimeout.call(globalThis, resolve, delay))
    }

    await new Promise<void>((resolve) => nativeTimers.setTimeout.call(globalThis, resolve, 0))
    if (!([...trackedDomTimeouts.values()].some((timer) => timer.timeout <= DOM_TIMER_DRAIN_TIMEOUT_MS)) && trackedDomIntervals.size === 0) return
  }
}

const originalWebSocket = (globalThis as { WebSocket?: unknown }).WebSocket
const originalXhr = (globalThis as { XMLHttpRequest?: unknown }).XMLHttpRequest

export function pendingDomTimers(): PendingDomTimer[] {
  return pendingTimers()
}

function registerDom(): void {
  if (!GlobalRegistrator.isRegistered) GlobalRegistrator.register({ url: 'http://localhost:5173' })
  Object.assign(globalThis, {
    ...nativeTimers,
    setTimeout: trackedSetTimeout,
    clearTimeout: trackedClearTimeout,
    setInterval: trackedSetInterval,
    clearInterval: trackedClearInterval,
  })
  const windowTimers = globalThis.window === globalThis
    ? {
        setTimeout: trackedSetTimeout,
        clearTimeout: trackedClearTimeout,
        setInterval: trackedSetInterval,
        clearInterval: trackedClearInterval,
      }
    : {
        setTimeout: (...args: Parameters<typeof setTimeout>) => globalThis.setTimeout(...args),
        clearTimeout: (...args: Parameters<typeof clearTimeout>) => globalThis.clearTimeout(...args),
        setInterval: (...args: Parameters<typeof setInterval>) => globalThis.setInterval(...args),
        clearInterval: (...args: Parameters<typeof clearInterval>) => globalThis.clearInterval(...args),
      }
  Object.assign(globalThis.window, {
    setTimeout: windowTimers.setTimeout as unknown as Window['setTimeout'],
    clearTimeout: windowTimers.clearTimeout as unknown as Window['clearTimeout'],
    setInterval: windowTimers.setInterval as unknown as Window['setInterval'],
    clearInterval: windowTimers.clearInterval as unknown as Window['clearInterval'],
  })
  globalThis.fetch = (async () => new Response('{}', { status: 200 })) as typeof fetch
  ;(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true
}

export async function setupDom(): Promise<void> {
  const previous = domLeaseTail
  let release!: () => void
  domLeaseTail = new Promise<void>((resolve) => { release = resolve })
  await previous
  releaseDomLease = release
  resetTranslateReady()
  try {
    registerDom()
  } catch (error) {
    releaseDomLease = null
    release()
    throw error
  }
}

interface MockBackend {
  request(method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE', path: string, body?: unknown): Promise<unknown>
  upload(path: string, file: Blob, opts?: { signal?: AbortSignal; onProgress?: (loaded: number, total: number) => void }): Promise<unknown>
}

export function mockApi(mock: MockBackend): FakeRequest[] {
  const requests: FakeRequest[] = []
  const statusOf = (e: unknown) => {
    const err = e as { status?: number; body?: unknown }
    return typeof err.status === 'number' ? { status: err.status, body: err.body ?? {} } : null
  }
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const method = (init?.method ?? 'GET') as 'GET'
    const path = String(input).replace(/^\/api/, '')
    const body = init?.body ? JSON.parse(init.body) : undefined
    requests.push({ method, path: String(input), body })
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    try {
      const out = await mock.request(method, path, body)
      return out === null || out === undefined ? new Response(null, { status: 204 }) : new Response(JSON.stringify(out), { status: 200 })
    } catch (e) {
      const st = statusOf(e)
      if (!st) throw e
      return new Response(JSON.stringify(st.body), { status: st.status })
    }
  }) as unknown as typeof fetch
  class FakeXHR {
    status = 0
    responseText = ''
    onload: (() => void) | null = null
    onerror: (() => void) | null = null
    onabort: (() => void) | null = null
    upload: { onprogress: ((e: unknown) => void) | null } = { onprogress: null }
    private url = ''
    open(_method: string, url: string) {
      this.url = url
    }
    setRequestHeader() {}
    abort() {
      this.onabort?.()
    }
    send(file: Blob) {
      requests.push({ method: 'POST', path: this.url, body: { upload: true, size: file.size } })
      mock.upload(this.url.replace(/^\/api/, ''), file).then(
        (out) => {
          this.status = 200
          this.responseText = JSON.stringify(out)
          this.onload?.()
        },
        (e) => {
          const st = statusOf(e)
          this.status = st?.status ?? 500
          this.responseText = JSON.stringify(st?.body ?? {})
          this.onload?.()
        },
      )
    }
  }
  ;(globalThis as { XMLHttpRequest?: unknown }).XMLHttpRequest = FakeXHR
  return requests
}

export interface FakeSocketControl {
  readonly live: () => number
  readonly connects: () => number
  open(): Promise<void>
  drop(): Promise<void>
  message(obj: unknown): Promise<void>
  sent(): unknown[]
}

export function fakeWebSocket(): FakeSocketControl {
  const all: FakeWS[] = []
  class FakeWS {
    static readonly CONNECTING = 0
    static readonly OPEN = 1
    static readonly CLOSING = 2
    static readonly CLOSED = 3
    readyState = 0
    onopen: (() => void) | null = null
    onmessage: ((ev: { data: string }) => void) | null = null
    onerror: (() => void) | null = null
    onclose: (() => void) | null = null
    readonly url: string
    readonly sent: unknown[] = []
    constructor(url: string) {
      this.url = url
      all.push(this)
    }
    send(data: string) {
      this.sent.push(JSON.parse(data))
    }
    close() {
      this.readyState = 3
    }
  }
  ;(globalThis as { WebSocket?: unknown }).WebSocket = FakeWS
  const latest = () => all[all.length - 1]
  return {
    live: () => all.filter((w) => w.readyState < 2).length,
    connects: () => all.length,
    async open() {
      const w = latest()
      await act(async () => {
        w.readyState = 1
        w.onopen?.()
      })
    },
    async drop() {
      const w = latest()
      await act(async () => {
        w.readyState = 3
        w.onclose?.()
      })
    },
    async message(obj: unknown) {
      const w = latest()
      await act(async () => {
        w.onmessage?.({ data: JSON.stringify(obj) })
      })
    },
    sent: () => latest()?.sent ?? [],
  }
}

export async function typeInto(el: HTMLTextAreaElement | HTMLInputElement, text: string): Promise<void> {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype
  const setter = Object.getOwnPropertyDescriptor(proto, 'value')!.set!
  await act(async () => {
    setter.call(el, text)
    el.dispatchEvent(new Event('input', { bubbles: true }))
  })
}

export async function imeEnter(el: Element, how: 'composing' | 'keycode229' = 'composing'): Promise<KeyboardEvent> {
  const init: KeyboardEventInit & { keyCode?: number } = how === 'composing' ? { isComposing: true } : { keyCode: 229 }
  const event = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true, ...init })
  if (how === 'keycode229') Object.defineProperty(event, 'keyCode', { value: 229 })
  await act(async () => {
    el.dispatchEvent(event)
  })
  return event
}

export async function teardownDom(): Promise<void> {
  if (!releaseDomLease) return

  let cleanupError: unknown
  let hasCleanupError = false
  try {
    await unmountAll()
    await drainDomTimers()
  } catch (error) {
    cleanupError = error
    hasCleanupError = true
  } finally {
    clearPendingDomTimers()
    try {
      await GlobalRegistrator.unregister()
    } finally {
      Object.assign(globalThis, nativeTimers)
      globalThis.fetch = originalFetch
      ;(globalThis as { WebSocket?: unknown }).WebSocket = originalWebSocket
      ;(globalThis as { XMLHttpRequest?: unknown }).XMLHttpRequest = originalXhr
      ;(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = false
      const release = releaseDomLease
      releaseDomLease = null
      release?.()
    }
  }

  if (hasCleanupError) throw cleanupError
}

registerDom()
const React = await import('react')
const { createRoot } = await import('react-dom/client')

const mounted: Array<{ root: ReturnType<typeof createRoot>; host: HTMLElement }> = []

const ORPHAN_ACT_WAIT_MS = 5000
const inflightActs = new Map<Promise<unknown>, TestScope | undefined>()

export function act<T>(callback: () => T | Promise<T>): Promise<Awaited<T>> {
  const ended = endedTestScope()
  if (ended) return Promise.reject(new Error(`測試「${ended.name}」已經結束（多半是逾時），它的 body 還在跑：不再進 act()，以免跟後面的測試重疊`))
  const orphans = [...inflightActs].filter(([, scope]) => scope?.ended).map(([p]) => p)
  const run = () => {
    const p = Promise.resolve(React.act(callback) as PromiseLike<Awaited<T>>)
    const settled = p.then(
      () => {},
      () => {},
    )
    inflightActs.set(settled, currentTestScope())
    void settled.then(() => inflightActs.delete(settled))
    return p
  }
  if (!orphans.length) return run()
  let capTimer!: ReturnType<typeof setTimeout>
  const cap = new Promise((resolve) => {
    capTimer = setTimeout(resolve, ORPHAN_ACT_WAIT_MS)
  })
  return Promise.race([Promise.allSettled(orphans), cap]).finally(() => clearTimeout(capTimer)).then(run)
}

export async function mount(element: ReactElement): Promise<HTMLElement> {
  const host = document.createElement('div')
  document.body.appendChild(host)
  const root = createRoot(host)
  await act(async () => {
    root.render(element)
  })
  mounted.push({ root, host })
  return host
}

export async function unmountAll(): Promise<void> {
  if (!currentTestScope()) endCurrentTest()
  await settle()
  for (const { root, host } of mounted.splice(0)) {
    await act(async () => root.unmount())
    host.remove()
  }
  document.body.innerHTML = ''
}

export async function click(el: Element): Promise<void> {
  await act(async () => {
    for (const type of ['mousedown', 'mouseup', 'click']) {
      el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button: 0 }))
    }
  })
}

export async function keydown(el: Element, key: string, init: KeyboardEventInit = {}): Promise<KeyboardEvent> {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init })
  await act(async () => {
    el.dispatchEvent(event)
  })
  return event
}

export function focusLikeBrowserClick(el: Element): void {
  const target = el.closest<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')
  if (target) target.focus()
  else (document.activeElement as HTMLElement | null)?.blur()
}

export async function settle(ms = 0): Promise<void> {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, ms))
  })
}

export interface FakeRequest {
  method: string
  path: string
  body: unknown
}

export function fakeApi(route: (req: FakeRequest) => unknown = () => undefined): FakeRequest[] {
  const requests: FakeRequest[] = []
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    const req: FakeRequest = { method: init?.method ?? 'GET', path: String(input), body: init?.body ? JSON.parse(init.body) : undefined }
    requests.push(req)
    const out = route(req)
    return new Response(JSON.stringify(out ?? {}), { status: 200 })
  }) as unknown as typeof fetch
  return requests
}

export const POLL_TICK_MS = 250

let pollTick: (() => Promise<void>) | null = null

export function setPollTick(fn: (() => Promise<void>) | null): void {
  pollTick = fn
}

export async function until(cond: () => boolean | Promise<boolean>, what: string, ms = 8000): Promise<void> {
  const tick = pollTick
  if (!tick) {
    const end = Date.now() + ms
    while (Date.now() < end) {
      if (await cond()) return
      await settle(50)
    }
    throw new Error(`等不到：${what}`)
  }

  const start = Date.now()
  const end = start + ms
  let steps = 0
  const maxSteps = Math.ceil(ms / POLL_TICK_MS)

  for (;;) {
    if (await cond()) return
    const now = Date.now()
    if (steps >= maxSteps && now >= end) {
      const elapsed = now - start
      throw new Error(`等不到：${what}（輪詢 ${steps} 步、虛擬 ${steps * POLL_TICK_MS}ms、實時 ${elapsed}ms）`)
    }
    steps++
    await tick()
    await settle(10)
  }
}
