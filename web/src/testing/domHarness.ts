/**
 * 互動測試的跑道：把元件真的掛進 happy-dom，發鍵盤／滑鼠事件、斷言行為（不是掃原始碼）。
 *
 * 用法：測試檔第一個 import 這支，再 import 要測的元件；檔案裡 `before(setupDom)`、`after(teardownDom)`。
 * **DOM 只在 DOM 測試檔的前後存在**：別的測試檔自己 stub `localStorage`／`window`，全域 DOM 留著會讓它們壞掉
 * （實測：不拆的話 15 個無關測試失敗）。事件都在 `act` 裡發，React 的更新照真的流程跑完才回來。
 */
import { GlobalRegistrator } from '@happy-dom/global-registrator'
import { currentTestScope, endCurrentTest, endedTestScope } from './testScope'
import type { TestScope } from './testScope'
import type { ReactElement } from 'react'

const originalFetch = globalThis.fetch
// happy-dom 註冊會換掉 queueMicrotask／setTimeout／clearTimeout／setInterval，換成綁在那個 window 上的版本；window 一關，這些 timer 就被清掉。
// 兩個後果，整樹同一個行程時才現形（單檔都綠）：
// 1. React 載入時抓走第一個 window 的 `queueMicrotask`，該 window 拆掉後，之後每個 DOM 測試檔裡「不在 act 裡」的 store→render 更新就再也不會 flush。
// 2. 共用的 store／mock 在檔案結束時還有 `setTimeout` 在等（例如 mock 的 60ms 延遲）：timer 被清掉、promise 永遠不 resolve，
//    `singleFlight(refreshState)` 就永遠卡在那一趟，下一個檔案的 `refreshState()` 全部跟著卡死。
// 所以一律用原生的：timer 不隨 window 消失。
// DOM 測試中的 timer 使用原生計時器（不隨 happy-dom window 消失），但 teardown 前必須收束，不可讓 callback 跑進下一個測試。
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

/** 等待已排定的 timeout 與其 microtask 後續；無法收束時以來源測試及呼叫堆疊失敗。 */
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

    // 讓 timeout callback 排下的 Promise continuation 有機會再排出 timer；如有，回到上方繼續收束。
    await new Promise<void>((resolve) => nativeTimers.setTimeout.call(globalThis, resolve, 0))
    if (!([...trackedDomTimeouts.values()].some((timer) => timer.timeout <= DOM_TIMER_DRAIN_TIMEOUT_MS)) && trackedDomIntervals.size === 0) return
  }
}

const originalWebSocket = (globalThis as { WebSocket?: unknown }).WebSocket
const originalXhr = (globalThis as { XMLHttpRequest?: unknown }).XMLHttpRequest

/** 查詢目前尚未觸發的 DOM timer（除錯或守門斷言用）。 */
export function pendingDomTimers(): PendingDomTimer[] {
  return pendingTimers()
}

/** 安裝一個 happy-dom global；呼叫時必須持有 DOM test lease。 */
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
        // Happy-dom's `window` is separate from globalThis. Delegate dynamically so a test that replaces
        // global timers with a manual clock also controls timers scheduled through `window`.
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

/** 註冊全域 DOM（已經註冊就沿用）；同一 process 的 DOM 測試檔共用 global，必須持有互斥 lease。 */
export async function setupDom(): Promise<void> {
  const previous = domLeaseTail
  let release!: () => void
  domLeaseTail = new Promise<void>((resolve) => { release = resolve })
  await previous
  releaseDomLease = release
  try {
    registerDom()
  } catch (error) {
    releaseDomLease = null
    release()
    throw error
  }
}

/** `MockTransport` 的最小介面（`request`／`upload`）。 */
interface MockBackend {
  request(method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE', path: string, body?: unknown): Promise<unknown>
  upload(path: string, file: Blob, opts?: { signal?: AbortSignal; onProgress?: (loaded: number, total: number) => void }): Promise<unknown>
}

/**
 * 把真正的 `HttpTransport` 的 fetch／XHR 接到 `MockTransport`：前端走的是正式程式碼路徑（URL、標頭、錯誤碼解析），
 * 後端是 mock 的 daemon 行為。回傳 `requests`（每一筆都記下來，上傳走 XHR 也記）。
 * `ApiError` 會變成對應狀態碼與 body 的 HTTP 回應；上傳走假的 `XMLHttpRequest`。
 */
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
    // `/session` 是 daemon 發 token 的地方，不是 mock 的路由。
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
  /** 目前存活的連線數（`close()` 或被取代的不算）。 */
  readonly live: () => number
  /** 累計連過幾次。 */
  readonly connects: () => number
  /** 伺服器那頭接受連線：最新那條 `onopen`。 */
  open(): Promise<void>
  /** 連線掉了（心跳斷線／網路斷）：最新那條 `onclose`。 */
  drop(): Promise<void>
}

/** 假的 `WebSocket`：測試手動決定什麼時候連上、什麼時候掉線；`HttpTransport.openSocket` 的重連與 backoff 照真的跑。 */
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
    constructor(url: string) {
      this.url = url
      all.push(this)
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
  }
}

/** 在 `<textarea>`／`<input>` 打字：照 React 受控元件認得的方式（原生 setter ＋ `input` 事件）。 */
export async function typeInto(el: HTMLTextAreaElement | HTMLInputElement, text: string): Promise<void> {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype
  const setter = Object.getOwnPropertyDescriptor(proto, 'value')!.set!
  await act(async () => {
    setter.call(el, text)
    el.dispatchEvent(new Event('input', { bubbles: true }))
  })
}

/** 輸入法組字中的 Enter（確認選字）：`isComposing` 為真、`keyCode` 229（WebKit 在 compositionend 之後才送的那一個）。 */
export async function imeEnter(el: Element, how: 'composing' | 'keycode229' = 'composing'): Promise<KeyboardEvent> {
  const init: KeyboardEventInit & { keyCode?: number } = how === 'composing' ? { isComposing: true } : { keyCode: 229 }
  const event = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true, ...init })
  if (how === 'keycode229') Object.defineProperty(event, 'keyCode', { value: 229 })
  await act(async () => {
    el.dispatchEvent(event)
  })
  return event
}

/** 拆掉全域 DOM、還原 fetch／WebSocket／XHR，讓後面不碰 DOM 的測試檔回到原來的環境。 */
export async function teardownDom(): Promise<void> {
  if (!releaseDomLease) return

  let cleanupError: unknown
  let hasCleanupError = false
  try {
    // 先把還沒卸載的元件樹卸掉，避免在拆 window 時 unmount 觸發 DOM 操作衝突。
    // drain 留在檔案 teardown 邊界，避免每條測試的 afterEach 把測試刻意保留的 timer 當成洩漏。
    await unmountAll()
    await drainDomTimers()
  } catch (error) {
    cleanupError = error
    hasCleanupError = true
  } finally {
    // 長逾時不阻塞 teardown；成功或失敗都取消剩餘計時器，避免 callback 跑進下一個測試檔。
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

// `react-dom/client` 在載入時就看有沒有 `window`：要在有 DOM 的時候第一次載入。
registerDom()
const React = await import('react')
const { createRoot } = await import('react-dom/client')

const mounted: Array<{ root: ReturnType<typeof createRoot>; host: HTMLElement }> = []

const ORPHAN_ACT_WAIT_MS = 5000
/** 還沒結束的 `act()`，記著是哪條測試發的（hook 裡發的是 `undefined`）。 */
const inflightActs = new Map<Promise<unknown>, TestScope | undefined>()

/**
 * `React.act`，但不讓逾時測試留下來的 body 跟後面的測試重疊（#770，見 `testScope.ts`）：
 * - 已經結束的測試再呼叫：直接丟錯（訊息寫出是哪一條），它就停在這裡，不再動共用的 DOM／store。
 * - 已經結束的測試還有 `act()` 在半路：先等它收掉再開新的，React 的 act 深度才不會錯位。
 */
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
  // 等有上限：那個 act 永遠不收（卡在不會 resolve 的 promise）的話，寧可照跑、讓它紅在這裡，也不要整套測試掛住。
  let capTimer!: ReturnType<typeof setTimeout>
  const cap = new Promise((resolve) => {
    capTimer = setTimeout(resolve, ORPHAN_ACT_WAIT_MS)
  })
  return Promise.race([Promise.allSettled(orphans), cap]).finally(() => clearTimeout(capTimer)).then(run)
}

/** 掛進 body，回容器。 */
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

/** 每個測試結束時呼叫：卸掉所有掛上去的樹、清空 body，下一個測試從乾淨的 DOM 開始。 */
export async function unmountAll(): Promise<void> {
  // 從 afterEach 叫的（不在任何測試 body 裡）：逾時的那條在這之後就進不了 act()（shim 的 afterEach 也會標，但順序不保證在這之前）。
  // 測試 body 中途自己叫（例如模擬換一個分頁）不算結束。
  if (!currentTestScope()) endCurrentTest()
  await settle() // 還在路上的 fetch→setState 先跑完，卸載之後才不會有更新打到已卸掉的樹
  for (const { root, host } of mounted.splice(0)) {
    await act(async () => root.unmount())
    host.remove()
  }
  document.body.innerHTML = ''
}

/** 一次滑鼠點擊：mousedown → mouseup → click，都會冒泡。 */
export async function click(el: Element): Promise<void> {
  await act(async () => {
    for (const type of ['mousedown', 'mouseup', 'click']) {
      el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button: 0 }))
    }
  })
}

/** 在 `el` 上發一個 keydown，回事件（看 `defaultPrevented`）。 */
export async function keydown(el: Element, key: string, init: KeyboardEventInit = {}): Promise<KeyboardEvent> {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init })
  await act(async () => {
    el.dispatchEvent(event)
  })
  return event
}

/** 照瀏覽器的預設：點到非可聚焦的內容，焦點移到最近的可聚焦祖先（沒有就掉到 body）。 */
export function focusLikeBrowserClick(el: Element): void {
  const target = el.closest<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')
  if (target) target.focus()
  else (document.activeElement as HTMLElement | null)?.blur()
}

/** 等一輪 microtask／timer（元件掛上去後的 fetch→setState 跑完）。 */
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

/**
 * 換掉全域 fetch：每筆請求記進 `requests`，`route` 決定回什麼（回 `undefined` ＝ 空物件 200）。
 * 回傳 `requests`，測試用它斷言「有沒有打、打了什麼」。
 */
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

/** 設定 `until` 每輪輪詢要做的事（例如推進 mock 的虛擬時鐘）；`null` 取消。 */
export function setPollTick(fn: (() => Promise<void>) | null): void {
  pollTick = fn
}

/**
 * 輪詢到條件成立；逾時就用 `what` 當失敗訊息。有 `setPollTick` 時每輪先推進虛擬時間再讓出一小段真時間：
 * 「等 mock 的 8 秒回合結束」變成「輪詢幾次」，不再取決於這台機器現在多忙。
 */
export async function until(cond: () => boolean | Promise<boolean>, what: string, ms = 8000): Promise<void> {
  const tick = pollTick
  if (!tick) {
    // 協調者要求：沒有 pollTick 時行為與原實作一個 byte 都不差（包含先判 Date.now() < end）。
    const end = Date.now() + ms
    while (Date.now() < end) {
      if (await cond()) return
      await settle(50)
    }
    throw new Error(`等不到：${what}`)
  }

  // 進函式時固定使用讀到的 tick（維持進函式時讀一次），有 pollTick 時保證慢機器上步數推進到 maxSteps。
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
