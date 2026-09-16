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
  let inflight: Promise<void> | null = null
  let timer: unknown = null
  let stopWake: (() => void) | null = null
  const subscribers = new Set<(u: HerdrUpdates) => void>()

  const publish = (u: HerdrUpdates) => {
    cached = u
    for (const cb of subscribers) cb(u)
  }

  /** 連不上 daemon。有舊資料就留著，只有從來沒拿到過才畫「什麼都不知道」。 */
  const onError = (e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e)
    const status = e instanceof HerdrUpdatesError ? e.status : null
    // 只有 404 代表「這個 daemon 沒有這個功能」；其他都是暫時連不上。
    publish(cached ? degraded(cached, msg) : emptyUpdates(msg, status === 404))
  }

  const run = (call: () => Promise<HerdrUpdates>): Promise<void> => {
    if (inflight) return inflight
    inflight = call()
      // 「上次對上 daemon 的時間」在這裡蓋：失敗那條路不會更新它，所以下一拍還會再試。
      .then((u) => publish({ ...u, syncedAt: deps.now() }), onError)
      .finally(() => {
        inflight = null
      })
    return inflight
  }

  const load = (force = false): Promise<void> => {
    const at = cached?.syncedAt
    if (!force && at != null && deps.now() - at < FRESH_MS) return Promise.resolve()
    return run(deps.fetchCache)
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
    refresh: () => run(deps.refreshNow),
    markSeenNow: () => {
      const version = cached?.latest.version
      if (!version) return Promise.resolve()
      return run(() => deps.markSeen(version))
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
