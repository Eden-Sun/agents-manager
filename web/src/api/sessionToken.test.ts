import test from 'node:test'
import assert from 'node:assert/strict'
import { TOKEN_KEY, deviceTokenStore, memoryTokenStore } from './sessionToken.ts'

/** 換掉 `localStorage`，回復函式在最後叫。 */
function withStorage(fake: unknown): () => void {
  const g = globalThis as unknown as Record<string, unknown>
  const had = Object.prototype.hasOwnProperty.call(g, 'localStorage')
  const prev = g.localStorage
  Object.defineProperty(g, 'localStorage', { value: fake, configurable: true, writable: true })
  return () => {
    if (had) Object.defineProperty(g, 'localStorage', { value: prev, configurable: true, writable: true })
    else delete g.localStorage
  }
}

function fakeStorage() {
  const map = new Map<string, string>()
  return {
    map,
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  }
}

test('token 存得進 localStorage，也讀得回來、清得掉', () => {
  const fake = fakeStorage()
  const restore = withStorage(fake)
  try {
    const store = deviceTokenStore()
    assert.equal(store.get(), '')

    store.set('abc123')
    assert.equal(fake.map.get(TOKEN_KEY), 'abc123')
    // 重整＝新的一個 store 實例，還是要讀得到同一把。
    assert.equal(deviceTokenStore().get(), 'abc123')

    store.clear()
    assert.equal(fake.map.has(TOKEN_KEY), false)
    assert.equal(deviceTokenStore().get(), '')
  } finally {
    restore()
  }
})

test('localStorage 會丟例外時退回記憶體，不讓整個 App 開不起來', () => {
  const restore = withStorage({
    getItem: () => {
      throw new Error('blocked')
    },
    setItem: () => {
      throw new Error('blocked')
    },
    removeItem: () => {
      throw new Error('blocked')
    },
  })
  try {
    const store = deviceTokenStore()
    assert.equal(store.get(), '')
    store.set('abc123')
    // 存不下去，但這一頁還是用得到那把 token。
    assert.equal(store.get(), 'abc123')
    store.clear()
    assert.equal(store.get(), '')
  } finally {
    restore()
  }
})

test('memoryTokenStore 就只是一個變數', () => {
  const store = memoryTokenStore('seed')
  assert.equal(store.get(), 'seed')
  store.set('next')
  assert.equal(store.get(), 'next')
  store.clear()
  assert.equal(store.get(), '')
})
