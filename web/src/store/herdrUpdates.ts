/**
 * Herdr 版本卡的共用狀態。
 *
 * 兩件事讓「掛上去抓一次就算了」不夠用：
 *
 * 1. daemon 的 watcher 是**啟動後才**去查官方版本的，所以頁面一開就抓的那一次很可能拿到空的快取。
 *    不再抓第二次的話，那個分頁到關掉為止都不會知道有新版。
 * 2. 這個頁面會開著好幾天。6 小時後 daemon 查到新版，畫面不該要人重整才看得到。
 *
 * 所以這裡低頻輪詢 **`GET`（只讀 daemon 的快取，不會叫它去打官方站）**，加上「分頁重新可見」與
 * 「網路回來」時補一次。背景分頁不輪詢，最後一個訂閱者離開就把 timer 收掉。
 *
 * 失敗時**保留上一份好資料**，只標成過期並寫上原因：一次網路抖動不該把整張卡清成「什麼都不知道」，
 * 那跟 daemon 那邊「抓失敗不吞舊快取」的規則自相矛盾，也會讓已經亮出來的「有新版」憑空消失。
 *
 * 讀與使用者動作**不共用 in-flight**：合流只對「讀」成立，按鈕一定要真的送出去。
 *
 * 依賴都從外面傳進來（計時器、可見性、三支 API），測試才能不靠真的 timer 把這些行為跑完。
 */
import type { HerdrUpdates } from '../api/herdrUpdates'
import { degraded, emptyUpdates, HerdrUpdatesError } from '../api/herdrUpdates'

/** 只讀 daemon 快取的輪詢間隔。不是去打官方站，所以便宜。 */
export const POLL_MS = 5 * 60 * 1000
/** 比這個新就不必為了「有人打開面板」再抓一次。 */
export const FRESH_MS = 30 * 1000

export interface StoreDeps {
  fetchCache: () => Promise<HerdrUpdates>
  refreshNow: () => Promise<HerdrUpdates>
  markSeen: (version: string) => Promise<HerdrUpdates>
  setInterval: (fn: () => void, ms: number) => unknown
  clearInterval: (handle: unknown) => void
  now: () => number
  /** 分頁重新可見 / 網路回來。回傳解除註冊。 */
  onWake: (fn: () => void) => () => void
  isVisible: () => boolean
}

export interface HerdrUpdatesStore {
  subscribe: (cb: (u: HerdrUpdates) => void) => () => void
  get: () => HerdrUpdates | null
  /** 拉一次 daemon 快取。`force` 略過「剛拿過就不再拿」的判斷。 */
  load: (force?: boolean) => Promise<void>
  /** 叫 daemon 真的去查官方站。 */
  refresh: () => Promise<void>
  markSeenNow: () => Promise<void>
}

export function createHerdrUpdatesStore(deps: StoreDeps): HerdrUpdatesStore {
  let cached: HerdrUpdates | null = null
  /** 進行中的 `GET`。只有讀會合流——讀誰先誰後都一樣。 */
  let readInflight: Promise<void> | null = null
  /** 使用者動作排成一列，彼此不交錯。 */
  let writes: Promise<unknown> = Promise.resolve()
  /** 送出的第幾個請求 / 最後一個寫進 `cached` 的請求。舊請求的回應不准蓋掉新的。 */
  let started = 0
  let published = 0
  let timer: unknown = null
  let stopWake: (() => void) | null = null
  const subscribers = new Set<(u: HerdrUpdates) => void>()

  const publish = (u: HerdrUpdates) => {
    cached = u
    for (const cb of subscribers) cb(u)
  }

  /**
   * 這個請求的結果還算不算數。
   *
   * 送出得早、回來得晚的 `GET`，不可以把它出發之後才完成的 POST 結果蓋掉——否則按了「知道了」、
   * 未讀消掉，一個更早出發的輪詢回來又把它點亮。
   */
  const apply = (seq: number, next: HerdrUpdates) => {
    if (seq < published) return
    published = seq
    publish(next)
  }

  const applyError = (seq: number, e: unknown) => {
    if (seq < published) return
    const msg = e instanceof Error ? e.message : String(e)
    const status = e instanceof HerdrUpdatesError ? e.status : null
    published = seq
    // 有舊資料就留著，只有從來沒拿到過才畫「什麼都不知道」；只有 404 代表這個 daemon 沒這功能。
    publish(cached ? degraded(cached, msg) : emptyUpdates(msg, status === 404))
  }

  const settle = (seq: number, call: () => Promise<HerdrUpdates>): Promise<void> =>
    call().then(
      (u) => apply(seq, { ...u, syncedAt: deps.now() }),
      (e) => applyError(seq, e),
    )

  /** 讀 daemon 快取。同時來的讀合流成一個請求。 */
  const load = (force = false): Promise<void> => {
    const at = cached?.syncedAt
    if (!force && at != null && deps.now() - at < FRESH_MS) return Promise.resolve()
    if (readInflight) return readInflight
    const seq = ++started
    readInflight = settle(seq, deps.fetchCache).finally(() => {
      readInflight = null
    })
    return readInflight
  }

  /**
   * 使用者動作（重新檢查、知道了）。**一定會送出**：跟讀共用一個 in-flight 的話，剛好有一輪輪詢在
   * 飛就會被靜靜吞掉——根本沒呼叫 POST，介面卻像成功了。彼此之間照按下的順序排隊。
   */
  const write = (call: () => Promise<HerdrUpdates>): Promise<void> => {
    // `++started` 在輪到它**真正送出**時才取號，順序才跟實際發出的順序一致。
    const p = writes.then(() => settle(++started, call))
    writes = p.catch(() => {})
    return p
  }

  const start = () => {
    if (timer === null) {
      // 背景分頁不打擾 daemon；回到前景時 `onWake` 會補一次。
      timer = deps.setInterval(() => {
        if (deps.isVisible()) void load(true)
      }, POLL_MS)
    }
    stopWake ??= deps.onWake(() => void load(true))
  }

  const stop = () => {
    if (timer !== null) {
      deps.clearInterval(timer)
      timer = null
    }
    stopWake?.()
    stopWake = null
  }

  return {
    get: () => cached,
    load,
    refresh: () => write(deps.refreshNow),
    markSeenNow: () => {
      // 版本綁在**按下去的那一刻**。排隊時如果新 release 到了，不能把新版誤標成已讀。
      const version = cached?.latest.version
      if (!version) return Promise.resolve()
      return write(() => deps.markSeen(version))
    },
    subscribe: (cb) => {
      subscribers.add(cb)
      start()
      // 有人打開面板時順便確認一次，但剛拿過就不必。
      void load()
      return () => {
        subscribers.delete(cb)
        if (subscribers.size === 0) stop()
      }
    },
  }
}
