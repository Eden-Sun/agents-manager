/**
 * Herdr 版本 store 的行為測試。
 *
 * 問的都是同一類問題：**畫面會不會停在一個過時或憑空消失的答案上**。
 * 計時器與可見性都是注入的假的，所以整份測試不用等真的 5 分鐘，也不需要瀏覽器。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { createHerdrUpdatesStore, FRESH_MS, POLL_MS } from './herdrUpdates.ts'
import type { StoreDeps } from './herdrUpdates.ts'
import { HerdrUpdatesError, toUpdates } from '../api/herdrUpdates.ts'
import type { HerdrUpdates } from '../api/herdrUpdates.ts'

interface Rig {
  deps: StoreDeps
  /** 跑一次輪詢的那一拍。 */
  tick: () => void
  /** 分頁重新可見／網路回來。 */
  wake: () => void
  calls: { get: number; refresh: number; seen: string[] }
  /** 下一次 `GET` 要回什麼；丟出去的東西會被當成失敗。 */
  next: (payload: unknown) => void
  timers: number
  wakeListeners: number
  clock: { ms: number }
  setVisible: (v: boolean) => void
}

function emptyPayload() {
  return { latest: { version: null, stale: true }, hosts: [], unread: false }
}

function behindPayload(version = '0.9.0') {
  return {
    latest: { version, stale: false, fetched_at: new Date().toISOString() },
    hosts: [
      {
        host: 'local',
        connected: true,
        fresh: true,
        server: { version: '0.8.2', fresh: true, standing: 'behind', cached_standing: 'behind' },
        disk: { version: '0.8.2', fresh: true, standing: 'behind', cached_standing: 'behind' },
      },
    ],
    unread: true,
  }
}

function rig(first: unknown = emptyPayload()): Rig {
  const state = { payload: first as unknown }
  const calls = { get: 0, refresh: 0, seen: [] as string[] }
  const clock = { ms: 1_000_000 }
  let visible = true
  let ticker: (() => void) | null = null
  let waker: (() => void) | null = null
  const r: Rig = {
    calls,
    clock,
    timers: 0,
    wakeListeners: 0,
    next: (payload) => {
      state.payload = payload
    },
    setVisible: (v) => {
      visible = v
    },
    tick: () => ticker?.(),
    wake: () => waker?.(),
    deps: {
      fetchCache: async () => {
        calls.get += 1
        if (state.payload instanceof Error) throw state.payload
        return toUpdates(state.payload)
      },
      refreshNow: async () => {
        calls.refresh += 1
        if (state.payload instanceof Error) throw state.payload
        return toUpdates(state.payload)
      },
      markSeen: async (v) => {
        calls.seen.push(v)
        if (state.payload instanceof Error) throw state.payload
        return toUpdates(state.payload)
      },
      setInterval: (fn) => {
        r.timers += 1
        ticker = fn
        return 'handle'
      },
      clearInterval: () => {
        r.timers -= 1
        ticker = null
      },
      now: () => clock.ms,
      onWake: (fn) => {
        r.wakeListeners += 1
        waker = fn
        return () => {
          r.wakeListeners -= 1
          waker = null
        }
      },
      isVisible: () => visible,
    },
  }
  return r
}

/** 等 store 內部那串 promise 跑完。 */
const settle = () => new Promise((r) => setTimeout(r, 0))

test('daemon 還沒查到版本時先拿到空的，之後不用重整就看得到新版', async () => {
  // 這是實際會發生的順序：daemon 的 watcher 起來 20 秒後才去查官方站，
  // 頁面一開就抓的那一次一定是空的。
  const r = rig(emptyPayload())
  const store = createHerdrUpdatesStore(r.deps)
  const seen: (HerdrUpdates | null)[] = []
  store.subscribe((u) => seen.push(u))
  await settle()
  assert.equal(store.get()?.latest.version, null, '第一次就是什麼都還不知道')

  // watcher 查到了 0.9.0。
  r.next(behindPayload())
  r.clock.ms += POLL_MS
  r.tick()
  await settle()
  assert.equal(store.get()?.latest.version, '0.9.0', '輪詢要把它補上來')
  assert.equal(store.get()?.unread, true)
  assert.deepEqual(store.get()?.behindHosts, ['local'])
  assert.ok(seen.length >= 2, '訂閱者要收到第二份')
})

test('分頁重新可見／網路回來時補一次，背景分頁不打擾 daemon', async () => {
  const r = rig(emptyPayload())
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  const afterMount = r.calls.get

  // 背景分頁：那一拍不發請求。
  r.setVisible(false)
  r.clock.ms += POLL_MS
  r.tick()
  await settle()
  assert.equal(r.calls.get, afterMount, '看不到的分頁不輪詢')

  // 回到前景。
  r.next(behindPayload())
  r.setVisible(true)
  r.wake()
  await settle()
  assert.equal(r.calls.get, afterMount + 1)
  assert.equal(store.get()?.latest.version, '0.9.0')
})

test('最後一個訂閱者離開就收掉 timer 與監聽，再訂閱會重開', async () => {
  const r = rig()
  const store = createHerdrUpdatesStore(r.deps)
  const off1 = store.subscribe(() => {})
  const off2 = store.subscribe(() => {})
  await settle()
  assert.equal(r.timers, 1, '兩個訂閱者共用一個 timer')
  assert.equal(r.wakeListeners, 1)

  off1()
  assert.equal(r.timers, 1, '還有人在看就不能收')
  off2()
  assert.equal(r.timers, 0, '沒人看了就收掉')
  assert.equal(r.wakeListeners, 0)

  store.subscribe(() => {})
  await settle()
  assert.equal(r.timers, 1, '再有人看就重開')
})

test('同時掛上來的訂閱者共用一次請求，兩邊都拿到資料', async () => {
  const r = rig(behindPayload())
  const store = createHerdrUpdatesStore(r.deps)
  const a: HerdrUpdates[] = []
  const b: HerdrUpdates[] = []
  store.subscribe((u) => a.push(u))
  store.subscribe((u) => b.push(u))
  await settle()
  assert.equal(r.calls.get, 1, '單飛：只打一次')
  assert.equal(a.at(-1)?.latest.version, '0.9.0')
  assert.equal(b.at(-1)?.latest.version, '0.9.0')
})

test('剛拿過就不為了「有人打開面板」再抓一次，過了就抓', async () => {
  const r = rig(behindPayload())
  const store = createHerdrUpdatesStore(r.deps)
  const off = store.subscribe(() => {})
  await settle()
  assert.equal(r.calls.get, 1)
  off()

  store.subscribe(() => {})
  await settle()
  assert.equal(r.calls.get, 1, '30 秒內再打開面板不必重抓')

  const off3 = store.subscribe(() => {})
  off3()
  r.clock.ms += FRESH_MS + 1
  store.subscribe(() => {})
  await settle()
  assert.equal(r.calls.get, 2, '資料放久了，打開面板要確認一次')
})

test('API 暫時斷線保留上一份好資料，只標成過期——未讀不會憑空消失', async () => {
  const r = rig(behindPayload())
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  assert.equal(store.get()?.unread, true)

  r.next(new HerdrUpdatesError('fetch failed', null))
  r.clock.ms += POLL_MS
  r.tick()
  await settle()
  const after = store.get()
  assert.equal(after?.latest.version, '0.9.0', '版本還在')
  assert.deepEqual(after?.behindHosts, ['local'], '落後的主機還在')
  assert.equal(after?.unread, true, '已經亮出來的未讀不能被一次網路抖動吃掉')
  assert.equal(after?.latest.stale, true, '但要標成舊資料')
  assert.equal(after?.error, 'fetch failed')
  assert.equal(after?.unsupported, false, '連不上不等於沒有這個功能')

  // 恢復之後回到正常。
  r.next(behindPayload())
  r.clock.ms += POLL_MS
  r.tick()
  await settle()
  assert.equal(store.get()?.error, null)
  assert.equal(store.get()?.latest.stale, false)
})

test('只有 404 才叫「這個 daemon 不支援」，而且不能蓋掉已經有的資料', async () => {
  const cold = rig(new HerdrUpdatesError('GET /herdr/updates failed (404)', 404))
  const store = createHerdrUpdatesStore(cold.deps)
  store.subscribe(() => {})
  await settle()
  assert.equal(store.get()?.unsupported, true)
  assert.equal(store.get()?.latest.version, null)

  // 500 不是「不支援」。
  const warm = rig(new HerdrUpdatesError('boom', 500))
  const s2 = createHerdrUpdatesStore(warm.deps)
  s2.subscribe(() => {})
  await settle()
  assert.equal(s2.get()?.unsupported, false)

  // 已經有資料之後才收到 404：資料留著，不憑空清空整張卡。
  const later = rig(behindPayload())
  const s3 = createHerdrUpdatesStore(later.deps)
  s3.subscribe(() => {})
  await settle()
  later.next(new HerdrUpdatesError('GET /herdr/updates failed (404)', 404))
  later.clock.ms += POLL_MS
  later.tick()
  await settle()
  assert.equal(s3.get()?.latest.version, '0.9.0', '拿過的資料不因為一次 404 蒸發')
  assert.equal(s3.get()?.error, 'GET /herdr/updates failed (404)')
})

test('按「知道了」失敗不會清空整張卡，也不會把未讀吃掉', async () => {
  const r = rig(behindPayload())
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()

  r.next(new HerdrUpdatesError('seen failed', 500))
  await store.markSeenNow()
  assert.deepEqual(r.calls.seen, ['0.9.0'], '送的是目前這一版')
  const after = store.get()
  assert.equal(after?.latest.version, '0.9.0')
  assert.equal(after?.unread, true, '沒標成功就還是未讀')
  assert.equal(after?.error, 'seen failed')
})

test('還不知道是哪一版時不會送出 seen', async () => {
  const r = rig(emptyPayload())
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  await store.markSeenNow()
  assert.deepEqual(r.calls.seen, [])
})

test('手動重新檢查走 refresh 端點，失敗一樣保留舊資料', async () => {
  const r = rig(behindPayload())
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()

  r.next(behindPayload('0.9.1'))
  await store.refresh()
  assert.equal(r.calls.refresh, 1)
  assert.equal(store.get()?.latest.version, '0.9.1')

  r.next(new HerdrUpdatesError('nope', 502))
  await store.refresh()
  assert.equal(store.get()?.latest.version, '0.9.1', '重查失敗不吞掉剛拿到的')
  assert.equal(store.get()?.error, 'nope')
})

// ---------------------------------------------------------------------------
// 請求排隊：讀可以合流，使用者動作不可以被合流掉。用 deferred promise 把「還沒回來」
// 這個狀態真的做出來，而不是靠時序運氣。
// ---------------------------------------------------------------------------

interface Deferred {
  settle: (payload: unknown) => void
  fail: (e: unknown) => void
  promise: Promise<HerdrUpdates>
}

interface SlowRig {
  deps: StoreDeps
  gets: Deferred[]
  refreshes: Deferred[]
  seens: { version: string; d: Deferred }[]
}

function defer(): Deferred {
  let settle!: (p: unknown) => void
  let fail!: (e: unknown) => void
  const promise = new Promise<HerdrUpdates>((res, rej) => {
    settle = (p: unknown) => res(toUpdates(p))
    fail = rej
  })
  return { settle, fail, promise }
}

/** 三支 API 都「掛著不回」，由測試決定什麼時候回、回什麼。 */
function slowRig(): SlowRig {
  const gets: Deferred[] = []
  const refreshes: Deferred[] = []
  const seens: { version: string; d: Deferred }[] = []
  return {
    gets,
    refreshes,
    seens,
    deps: {
      fetchCache: () => {
        const d = defer()
        gets.push(d)
        return d.promise
      },
      refreshNow: () => {
        const d = defer()
        refreshes.push(d)
        return d.promise
      },
      markSeen: (version) => {
        const d = defer()
        seens.push({ version, d })
        return d.promise
      },
      setInterval: () => 'handle',
      clearInterval: () => {},
      now: () => 1_000_000,
      onWake: () => () => {},
      isVisible: () => true,
    },
  }
}

test('GET 還沒回來時按「知道了」，POST 真的送出去，不是被合流掉當成功', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  assert.equal(r.gets.length, 1, '掛載時的那一次 GET 還掛著')

  // 先讓畫面上有一版可按。
  r.gets[0]?.settle(behindPayload('0.9.0'))
  await settle()
  // 再開一輪 GET（模擬輪詢），故意不讓它回來。
  void store.load(true)
  await settle()
  assert.equal(r.gets.length, 2, '第二次 GET 在飛')

  void store.markSeenNow()
  await settle()
  assert.equal(r.seens.length, 1, 'seen 必須真的送出，不能被進行中的 GET 吞掉')
  assert.equal(r.seens[0]?.version, '0.9.0')
})

test('GET 還沒回來時按「重新檢查」，refresh 也真的送出去', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  void store.refresh()
  await settle()
  assert.equal(r.refreshes.length, 1, 'refresh 不能被掛著的 GET 合流掉')
  assert.equal(r.gets.length, 1, '而且不會變成第二次 GET')
})

test('出發較早的 GET 後到，不能把已完成的 seen 結果蓋回未讀', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  r.gets[0]?.settle(behindPayload('0.9.0'))
  await settle()
  assert.equal(store.get()?.unread, true)

  // 舊的輪詢在飛（它看到的還是「未讀」）。
  void store.load(true)
  await settle()
  // 使用者按掉，POST 先回來。
  void store.markSeenNow()
  await settle()
  r.seens[0]?.d.settle({ ...behindPayload('0.9.0'), unread: false })
  await settle()
  assert.equal(store.get()?.unread, false, '按掉之後是已讀')

  // 那個更早出發的 GET 現在才回來，帶著過時的「未讀」。
  r.gets[1]?.settle(behindPayload('0.9.0'))
  await settle()
  assert.equal(store.get()?.unread, false, '舊回應不能把已讀翻回未讀')
})

test('seen 綁按下去那一版：排隊時新 release 到了也不會誤標新版', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  r.gets[0]?.settle(behindPayload('0.9.0'))
  await settle()

  // 先讓寫入佇列忙著（重新檢查還沒回來）。
  void store.refresh()
  await settle()
  assert.equal(r.refreshes.length, 1)

  // 使用者按下「知道了 0.9.0」，排在 refresh 後面。
  void store.markSeenNow()
  await settle()
  assert.equal(r.seens.length, 0, '還輪不到它送出')

  // 排隊期間官方出了 0.9.1，輪詢先把它帶進畫面。
  void store.load(true)
  await settle()
  r.gets[1]?.settle(behindPayload('0.9.1'))
  await settle()
  assert.equal(store.get()?.latest.version, '0.9.1')

  // 輪到 seen 送出：送的必須是按下去時看到的 0.9.0。
  r.refreshes[0]?.settle(behindPayload('0.9.1'))
  await settle()
  assert.equal(r.seens.length, 1)
  assert.equal(r.seens[0]?.version, '0.9.0', '不能把使用者沒看過的 0.9.1 標成已讀')
})

test('兩個使用者動作依序送出，不交錯', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  r.gets[0]?.settle(behindPayload('0.9.0'))
  await settle()

  void store.refresh()
  void store.markSeenNow()
  await settle()
  assert.equal(r.refreshes.length, 1)
  assert.equal(r.seens.length, 0, '第二個要等第一個結束')

  r.refreshes[0]?.settle(behindPayload('0.9.0'))
  await settle()
  assert.equal(r.seens.length, 1, '前一個結束才輪到它')
})

test('使用者動作失敗不會卡住後面的動作', async () => {
  const r = slowRig()
  const store = createHerdrUpdatesStore(r.deps)
  store.subscribe(() => {})
  await settle()
  r.gets[0]?.settle(behindPayload('0.9.0'))
  await settle()

  void store.refresh()
  await settle()
  r.refreshes[0]?.fail(new HerdrUpdatesError('boom', 500))
  await settle()
  assert.equal(store.get()?.error, 'boom')
  assert.equal(store.get()?.latest.version, '0.9.0', '失敗不清空')

  void store.markSeenNow()
  await settle()
  assert.equal(r.seens.length, 1, '前一個失敗了，後面的照樣送得出去')
})
