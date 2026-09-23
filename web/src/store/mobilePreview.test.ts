import test from 'node:test'
import assert from 'node:assert/strict'
import {
  readMobilePreviewOpen,
  setMobilePreviewOpen,
  subscribeMobilePreviewOpen,
  surveyDraftAllowed,
  writeShared,
} from './mobilePreview.ts'
import { saveCounts } from './unread.ts'

const MOBILE_PREVIEW_KEY = 'am.mobilePreview.open'
type TestStorage = { getItem: (key: string) => string | null; setItem: (key: string, value: string) => void }

function installWindow(storage: TestStorage) {
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'window')
  const listeners = new Map<string, Set<(event: Event) => void>>()
  const addEventListener = (type: string, listener: (event: Event) => void) => {
    const bucket = listeners.get(type) ?? new Set()
    bucket.add(listener)
    listeners.set(type, bucket)
  }
  const removeEventListener = (type: string, listener: (event: Event) => void) => {
    listeners.get(type)?.delete(listener)
  }
  const dispatchEvent = (event: Event) => {
    for (const listener of listeners.get(event.type) ?? []) listener(event)
    return true
  }
  const fakeWindow = { localStorage: storage, addEventListener, removeEventListener, dispatchEvent }
  Object.defineProperty(globalThis, 'window', { configurable: true, value: fakeWindow as unknown as Window })
  return {
    dispatchStorage(key: string | null, newValue: string | null) {
      const event = new Event('storage')
      Object.defineProperties(event, {
        key: { value: key },
        newValue: { value: newValue },
        storageArea: { value: storage },
      })
      dispatchEvent(event)
    },
    restore() {
      if (previous) Object.defineProperty(globalThis, 'window', previous)
      else Reflect.deleteProperty(globalThis, 'window')
    },
  }
}

test('手機預覽裡不寫共用的 localStorage：預覽的整份 map 不能蓋掉主畫面的（review3 c1 L8）', () => {
  const writes: string[] = []
  writeShared(() => writes.push('preview'), true)
  writeShared(() => writes.push('main'), false)
  assert.deepEqual(writes, ['main'])
})

test('主畫面照常寫（預設就是不在預覽裡）', () => {
  const map = new Map<string, string>()
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
  }
  saveCounts({ bots: { b1: 2 }, groups: {} })
  assert.equal(map.size, 1)
})

test('localStorage 寫入失敗時，這個分頁的 snapshot 仍反映開啟狀態', () => {
  let stored = '0'
  const mock = installWindow({
    getItem: () => stored,
    setItem: () => {
      throw new Error('storage is read-only')
    },
  })
  const observed: boolean[] = []
  const unsubscribe = subscribeMobilePreviewOpen(() => observed.push(readMobilePreviewOpen()))
  try {
    setMobilePreviewOpen(true)
    assert.equal(stored, '0', '無法寫入時持久值維持舊值')
    assert.equal(readMobilePreviewOpen(), true)
    assert.deepEqual(observed, [true], '同 tab 事件讀到的是記憶體 fallback')
  } finally {
    unsubscribe()
    mock.restore()
  }
})

test('另一個同源分頁或預覽 iframe 的 storage 事件可更新記憶體快照', () => {
  let stored = '0'
  const mock = installWindow({
    getItem: () => stored,
    setItem: () => {
      throw new Error('storage is read-only')
    },
  })
  const unsubscribe = subscribeMobilePreviewOpen(() => {})
  try {
    setMobilePreviewOpen(true)
    assert.equal(readMobilePreviewOpen(), true)

    stored = '1'
    mock.dispatchStorage(MOBILE_PREVIEW_KEY, '1')
    stored = '0'
    mock.dispatchStorage(MOBILE_PREVIEW_KEY, '0')
    assert.equal(readMobilePreviewOpen(), false, 'storage event 更新快照，不被舊 fallback 蓋住')
  } finally {
    unsubscribe()
    mock.restore()
  }
})

test('React 訂閱暫時卸載時，模組層 storage listener 仍同步 fallback', () => {
  let stored = '0'
  const mock = installWindow({
    getItem: () => stored,
    setItem: () => {
      throw new Error('storage is read-only')
    },
  })
  const unsubscribe = subscribeMobilePreviewOpen(() => {})
  try {
    setMobilePreviewOpen(true)
    assert.equal(readMobilePreviewOpen(), true)
    unsubscribe()

    stored = '1'
    mock.dispatchStorage(MOBILE_PREVIEW_KEY, '1')
    stored = '0'
    mock.dispatchStorage(MOBILE_PREVIEW_KEY, '0')
    assert.equal(readMobilePreviewOpen(), false, '重新掛載前 snapshot 已追上另一個 context')
  } finally {
    unsubscribe()
    mock.restore()
  }
})

test('storage 無法讀取時，也用跨分頁 storage event 的值更新 fallback', () => {
  const mock = installWindow({
    getItem: () => {
      throw new Error('storage is unavailable')
    },
    setItem: () => {
      throw new Error('storage is unavailable')
    },
  })
  const unsubscribe = subscribeMobilePreviewOpen(() => {})
  try {
    setMobilePreviewOpen(true)
    assert.equal(readMobilePreviewOpen(), true)
    mock.dispatchStorage(MOBILE_PREVIEW_KEY, '0')
    assert.equal(readMobilePreviewOpen(), false)
  } finally {
    unsubscribe()
    mock.restore()
  }
})

test('手機預覽裡的多分頁問卷不自動預載（預載會自己送導覽鍵，跟主畫面互相插隊）', () => {
  assert.equal(surveyDraftAllowed(true, true), false)
  assert.equal(surveyDraftAllowed(true, false), true)
  assert.equal(surveyDraftAllowed(false, false), false)
})
