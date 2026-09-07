/**
 * Keeps the image shelf across a reload: every parked `File` is mirrored into IndexedDB
 * (localStorage cannot hold blobs, and 24 × 12MB would not fit anyway) and read back on
 * the next page load. An item lives `SHELF_TTL_MS` (30 min) from the moment it was parked,
 * whether the tab was reloaded in between or not; expiry is checked on load and once a
 * minute while the page is open. Removing an item or clearing the shelf deletes it here too.
 *
 * All IndexedDB failures are swallowed: a private window or a blocked store just means the
 * shelf is back to memory-only, which is what it was before.
 */

import { SHELF_TTL_MS, useShelf } from './shelf'
import type { ShelfItem } from './shelf'

const DB_NAME = 'am-shelf'
const STORE = 'items'
const SWEEP_MS = 60 * 1000

interface Row {
  key: string
  name: string
  type: string
  blob: Blob
  addedAt: number
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    if (typeof indexedDB === 'undefined') return reject(new Error('no indexedDB'))
    const req = indexedDB.open(DB_NAME, 1)
    req.onupgradeneeded = () => {
      const db = req.result
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE, { keyPath: 'key' })
    }
    req.onsuccess = () => resolve(req.result)
    req.onerror = () => reject(req.error)
  })
}

function tx(mode: IDBTransactionMode, run: (store: IDBObjectStore) => void): Promise<void> {
  return openDb()
    .then(
      (db) =>
        new Promise<void>((resolve, reject) => {
          const t = db.transaction(STORE, mode)
          run(t.objectStore(STORE))
          t.oncomplete = () => resolve()
          t.onerror = () => reject(t.error)
          t.onabort = () => reject(t.error)
        }),
    )
    .catch(() => undefined)
}

function readAll(): Promise<Row[]> {
  return openDb()
    .then(
      (db) =>
        new Promise<Row[]>((resolve, reject) => {
          const req = db.transaction(STORE, 'readonly').objectStore(STORE).getAll()
          req.onsuccess = () => resolve(req.result as Row[])
          req.onerror = () => reject(req.error)
        }),
    )
    .catch(() => [] as Row[])
}

function put(items: ShelfItem[]): Promise<void> {
  return tx('readwrite', (store) => {
    for (const it of items) {
      const row: Row = { key: it.key, name: it.name, type: it.file.type, blob: it.file, addedAt: it.addedAt }
      store.put(row)
    }
  })
}

function del(keys: string[]): Promise<void> {
  return tx('readwrite', (store) => {
    for (const k of keys) store.delete(k)
  })
}

let started = false

/** Load what survived the last page, then mirror every change. Idempotent. */
export function startShelfPersistence(): void {
  if (started) return
  started = true

  void readAll().then((rows) => {
    const cutoff = Date.now() - SHELF_TTL_MS
    const live = rows.filter((r) => r.addedAt >= cutoff)
    const dead = rows.filter((r) => r.addedAt < cutoff).map((r) => r.key)
    if (dead.length) void del(dead)
    if (live.length) {
      useShelf.getState().restore(
        live
          .sort((a, b) => a.addedAt - b.addedAt)
          .map((r) => ({ key: r.key, file: new File([r.blob], r.name, { type: r.type }), addedAt: r.addedAt })),
      )
    }
  })

  let prev = useShelf.getState().items
  useShelf.subscribe((s) => {
    if (s.items === prev) return
    const before = new Set(prev.map((it) => it.key))
    const after = new Set(s.items.map((it) => it.key))
    const added = s.items.filter((it) => !before.has(it.key))
    const removed = prev.filter((it) => !after.has(it.key)).map((it) => it.key)
    prev = s.items
    if (added.length) void put(added)
    if (removed.length) void del(removed)
  })

  setInterval(() => useShelf.getState().expire(), SWEEP_MS)
}
