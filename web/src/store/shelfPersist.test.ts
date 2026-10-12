import test, { afterEach, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { IDBFactory } from 'fake-indexeddb'

import type { ShelfItem } from './shelf.ts'

// 真的走 `shelfPersist`（IndexedDB 用 fake-indexeddb 假實作）。每個「分頁」是一份獨立的 shelf 模組實例
// 加上一份獨立的 shelfPersist 模組實例，共用同一個假 DB（同源分頁共用 IndexedDB 的情況）。
// 「重新整理」＝開一個新的分頁（新模組實例，新的隨機前綴）去還原同一個 DB。

type Shelf = typeof import('./shelf.ts')
type Persist = typeof import('./shelfPersist.ts')

const DB_NAME = 'am-shelf'
const STORE = 'items'

let factory: IDBFactory
let savedIdb: unknown
let hadIdb: boolean
let seq = 0

beforeEach(() => {
  factory = new IDBFactory()
  hadIdb = 'indexedDB' in globalThis
  savedIdb = (globalThis as { indexedDB?: unknown }).indexedDB
  Object.assign(globalThis, { indexedDB: factory })
})

afterEach(() => {
  if (hadIdb) Object.assign(globalThis, { indexedDB: savedIdb })
  else Reflect.deleteProperty(globalThis, 'indexedDB')
})

/** 模組載入時 `Math.random()` 只呼叫一次，讓它吐出的 base36 前六位正好是 `tab`。 */
function tabRandom(tab: string): number {
  return (parseInt(tab, 36) + 0.25) / 36 ** 6
}

/** 用固定的隨機前綴載入一份獨立的 shelf 模組實例（同 `shelf.test.ts`）。 */
async function loadShelf(tab: string): Promise<Shelf> {
  const real = Math.random
  Math.random = () => tabRandom(tab)
  try {
    return (await import(`./shelf.ts?persist-${tab}-${++seq}`)) as Shelf
  } finally {
    Math.random = real
  }
}

/** 開一個分頁：獨立的 shelf 與獨立的 shelfPersist，persist 寫進共用的假 DB。 */
async function openPage(tab: string): Promise<Shelf> {
  const shelf = await loadShelf(tab)
  const persist = (await import(`./shelfPersist.ts?persist-${tab}-${++seq}`)) as Persist
  persist.startShelfPersistence(shelf.useShelf)
  return shelf
}

const file = (name: string, body = name) => new File([body], `${name}.pdf`, { type: 'application/pdf' })

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = factory.open(DB_NAME, 1)
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(STORE)) req.result.createObjectStore(STORE, { keyPath: 'key' })
    }
    req.onsuccess = () => resolve(req.result)
    req.onerror = () => reject(req.error)
  })
}

interface StoredRow {
  key: string
  name: string
  blob: Blob
  addedAt: number
}

/** 直接讀 DB 裡的列（不經過任何分頁），看的是真的寫進去的內容。 */
async function rows(): Promise<StoredRow[]> {
  const db = await openDb()
  return new Promise((resolve, reject) => {
    const req = db.transaction(STORE, 'readonly').objectStore(STORE).getAll()
    req.onsuccess = () => {
      db.close()
      resolve(req.result as StoredRow[])
    }
    req.onerror = () => reject(req.error)
  })
}

async function seed(list: StoredRow[]): Promise<void> {
  const db = await openDb()
  await new Promise<void>((resolve, reject) => {
    const t = db.transaction(STORE, 'readwrite')
    for (const r of list) t.objectStore(STORE).put(r)
    t.oncomplete = () => {
      db.close()
      resolve()
    }
    t.onerror = () => reject(t.error)
  })
}

/** 等非同步的寫入／還原跑到條件成立：每輪只讓出一次事件迴圈，不靠牆鐘；上限到了就失敗。 */
async function until(cond: () => boolean | Promise<boolean>, what: string): Promise<void> {
  for (let i = 0; i < 1000; i++) {
    if (await cond()) return
    await new Promise((r) => setTimeout(r, 0))
  }
  assert.fail(`逾時：${what}`)
}

const namesOf = (items: ShelfItem[]) => items.map((it) => it.name).sort()

async function rowKeys(): Promise<string[]> {
  return (await rows()).map((r) => r.key).sort()
}

test('批次加入的檔案寫進 DB，重新整理後原樣還原（含內容）', async () => {
  const a = await openPage('aaaaaa')
  a.useShelf.getState().add([file('a', 'AAA'), file('b', 'BBB')])
  await until(async () => (await rows()).length === 2, '批次加入寫入 DB')

  const b = await openPage('bbbbbb')
  await until(() => b.useShelf.getState().items.length === 2, '重新整理後還原')
  assert.deepEqual(namesOf(b.useShelf.getState().items), ['a.pdf', 'b.pdf'])
  const texts = await Promise.all(b.useShelf.getState().items.map((it) => it.file.text()))
  assert.deepEqual(texts.sort(), ['AAA', 'BBB'])
})

test('逐一加入的檔案都寫進 DB，重新整理後全部還原', async () => {
  const a = await openPage('aaaaaa')
  a.useShelf.getState().add([file('a')])
  a.useShelf.getState().add([file('b')])
  a.useShelf.getState().add([file('c')])
  await until(async () => (await rows()).length === 3, '逐一加入寫入 DB')

  const b = await openPage('bbbbbb')
  await until(() => b.useShelf.getState().items.length === 3, '重新整理後還原')
  assert.deepEqual(namesOf(b.useShelf.getState().items), ['a.pdf', 'b.pdf', 'c.pdf'])
})

test('刪除單筆後 DB 只少那一列，重新整理後不會回來', async () => {
  const a = await openPage('aaaaaa')
  a.useShelf.getState().add([file('a'), file('b')])
  await until(async () => (await rows()).length === 2, '加入寫入 DB')
  const gone = a.useShelf.getState().items[0]!.key
  a.useShelf.getState().remove(gone)
  await until(async () => !(await rowKeys()).includes(gone) && (await rows()).length === 1, '刪除寫入 DB')

  const b = await openPage('bbbbbb')
  await until(() => b.useShelf.getState().items.length === 1, '重新整理後還原')
  assert.deepEqual(namesOf(b.useShelf.getState().items), ['b.pdf'])
})

test('清空後 DB 是空的，重新整理後什麼都不還原', async () => {
  const a = await openPage('aaaaaa')
  a.useShelf.getState().add([file('a'), file('b')])
  await until(async () => (await rows()).length === 2, '加入寫入 DB')
  a.useShelf.getState().clear()
  await until(async () => (await rows()).length === 0, '清空寫入 DB')

  const b = await openPage('bbbbbb')
  // 還原是非同步的：讓它有機會跑完，再確認確實沒有東西回來。
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0))
  assert.equal(b.useShelf.getState().items.length, 0)
})

test('兩個分頁共用同一個 DB，各自新增的檔案互不覆蓋，各自刪除也不影響對方（#1619）', async () => {
  const tabA = await openPage('aaaaaa')
  const tabB = await openPage('bbbbbb')
  tabA.useShelf.getState().add([file('a', 'from-A')])
  tabB.useShelf.getState().add([file('b', 'from-B')])
  await until(async () => (await rows()).length === 2, '兩個分頁都寫進 DB')

  // 後寫的分頁不能蓋掉先寫的那一列：DB 裡兩列都在，內容也對得上。
  const stored = await rows()
  assert.deepEqual(stored.map((r) => r.name).sort(), ['a.pdf', 'b.pdf'])
  assert.deepEqual(await Promise.all(stored.map((r) => r.blob.text())).then((t) => t.sort()), ['from-A', 'from-B'])

  tabA.useShelf.getState().remove(tabA.useShelf.getState().items[0]!.key)
  await until(async () => (await rows()).length === 1, 'A 刪除自己的那一列')
  assert.deepEqual((await rows()).map((r) => r.name), ['b.pdf'])

  const tabC = await openPage('cccccc')
  await until(() => tabC.useShelf.getState().items.length === 1, '重新整理後還原')
  assert.deepEqual(namesOf(tabC.useShelf.getState().items), ['b.pdf'])
  const [only] = tabC.useShelf.getState().items
  assert.equal(await only!.file.text(), 'from-B')
})

test('舊版 s<n> 鍵與新格式鍵並存：兩者都還原，之後新增的鍵不與它們重複', async () => {
  await seed([
    { key: 's7', name: 'old.pdf', blob: file('old', 'OLD'), addedAt: Date.now() },
    { key: 'sabcdef1', name: 'new.pdf', blob: file('new', 'NEW'), addedAt: Date.now() },
  ])

  const next = await openPage('next00')
  await until(() => next.useShelf.getState().items.length === 2, '還原舊鍵與新鍵')
  next.useShelf.getState().add([file('x'), file('y')])
  await until(async () => (await rows()).length === 4, '新增寫入 DB')

  const keys = await rowKeys()
  assert.equal(new Set(keys).size, 4, `鍵重複：${keys.join(', ')}`)
  assert.ok(keys.includes('s7') && keys.includes('sabcdef1'), `舊鍵沒有保留：${keys.join(', ')}`)
})

test('IndexedDB 不存在時，頁面照常運作（只剩記憶體中的 shelf）', async () => {
  Reflect.deleteProperty(globalThis, 'indexedDB')
  const a = await openPage('aaaaaa')
  const res = a.useShelf.getState().add([file('a'), file('b')])
  assert.equal(res.added, 2)
  assert.equal(a.useShelf.getState().items.length, 2)
  a.useShelf.getState().remove(a.useShelf.getState().items[0]!.key)
  assert.equal(a.useShelf.getState().items.length, 1)
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0))
})

test('IndexedDB 開啟失敗（onerror）時，頁面照常運作', async () => {
  Object.assign(globalThis, {
    indexedDB: {
      open() {
        const req: { error: Error; onerror?: () => void; onsuccess?: () => void } = { error: new Error('denied') }
        queueMicrotask(() => req.onerror?.())
        return req
      },
    },
  })
  const a = await openPage('aaaaaa')
  assert.equal(a.useShelf.getState().add([file('a')]).added, 1)
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0))
  assert.equal(a.useShelf.getState().items.length, 1)
})

test('寫入／讀取丟例外時，頁面照常運作，記憶體中的 shelf 不受影響', async () => {
  const boom = () => {
    throw new Error('boom')
  }
  Object.assign(globalThis, {
    indexedDB: {
      open() {
        const req: { result?: unknown; error: Error; onupgradeneeded?: () => void; onsuccess?: () => void } = {
          error: new Error('unused'),
        }
        req.result = { objectStoreNames: { contains: () => true }, transaction: boom }
        queueMicrotask(() => req.onsuccess?.())
        return req
      },
    },
  })
  const a = await openPage('aaaaaa')
  assert.equal(a.useShelf.getState().add([file('a'), file('b')]).added, 2)
  a.useShelf.getState().remove(a.useShelf.getState().items[0]!.key)
  a.useShelf.getState().clear()
  assert.equal(a.useShelf.getState().items.length, 0)
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0))
})
