/**
 * Composer attachment draft persistence (IndexedDB + memory cache).
 *
 * Persists un-sent composer attachments across:
 * - Switching bots in the sidebar
 * - Browser reload / navigation away and back
 * - Mobile browser backgrounding and waking
 * - Send failures (allowing retry or bot switching without losing attached files)
 *
 * Cleared when:
 * - The turn is successfully sent (`clearSent`)
 * - The composer is cleared (`clear`)
 * - An attachment card is removed (`remove`)
 * - Attachments expire past 24 hours (`DRAFT_ATTACHMENT_TTL_MS`)
 * - Total draft storage exceeds `MAX_TOTAL_DRAFT_BYTES` (LRU eviction)
 */

export interface DraftAttachmentItem {
  key: string
  fp: string
  name: string
  size: number
  originalSize?: number
  isImage: boolean
  id: string | null
  file?: File | Blob
  addedAt: number
}

interface DraftAttachmentRow {
  /** Compound key: `${botId}:${key}` */
  id: string
  botId: string
  key: string
  fp: string
  name: string
  size: number
  originalSize?: number
  isImage: boolean
  attachmentId: string | null
  blob?: Blob
  addedAt: number
}

export const DRAFT_ATTACHMENT_DB_NAME = 'am-composer-attachment-drafts'
export const DRAFT_ATTACHMENT_STORE = 'drafts'
export const DRAFT_ATTACHMENT_TTL_MS = 24 * 60 * 60 * 1000 // 24 hours
export const MAX_TOTAL_DRAFT_BYTES = 100 * 1024 * 1024 // 100 MiB total
export const MAX_ITEMS_PER_BOT = 20
const SWEEP_INTERVAL_MS = 60 * 1000 // 1 minute

const memoryDrafts = new Map<string, DraftAttachmentItem[]>()
let dbLoaded = false
const loadListeners = new Set<() => void>()

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    if (typeof indexedDB === 'undefined') return reject(new Error('no indexedDB'))
    const req = indexedDB.open(DRAFT_ATTACHMENT_DB_NAME, 1)
    req.onupgradeneeded = () => {
      const db = req.result
      if (!db.objectStoreNames.contains(DRAFT_ATTACHMENT_STORE)) {
        const store = db.createObjectStore(DRAFT_ATTACHMENT_STORE, { keyPath: 'id' })
        store.createIndex('botId', 'botId', { unique: false })
        store.createIndex('addedAt', 'addedAt', { unique: false })
      }
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
          const t = db.transaction(DRAFT_ATTACHMENT_STORE, mode)
          run(t.objectStore(DRAFT_ATTACHMENT_STORE))
          t.oncomplete = () => resolve()
          t.onerror = () => reject(t.error)
          t.onabort = () => reject(t.error)
        }),
    )
    .catch(() => undefined)
}

function readAllRows(): Promise<DraftAttachmentRow[]> {
  return openDb()
    .then(
      (db) =>
        new Promise<DraftAttachmentRow[]>((resolve, reject) => {
          const req = db.transaction(DRAFT_ATTACHMENT_STORE, 'readonly').objectStore(DRAFT_ATTACHMENT_STORE).getAll()
          req.onsuccess = () => resolve((req.result as DraftAttachmentRow[]) || [])
          req.onerror = () => reject(req.error)
        }),
    )
    .catch(() => [] as DraftAttachmentRow[])
}

function putRow(row: DraftAttachmentRow): Promise<void> {
  return tx('readwrite', (store) => {
    store.put(row)
  })
}

function deleteRow(id: string): Promise<void> {
  return tx('readwrite', (store) => {
    store.delete(id)
  })
}

function deleteRows(ids: string[]): Promise<void> {
  if (ids.length === 0) return Promise.resolve()
  return tx('readwrite', (store) => {
    for (const id of ids) store.delete(id)
  })
}

function totalStoredBytes(): number {
  let total = 0
  for (const list of memoryDrafts.values()) {
    for (const it of list) {
      total += it.size || 0
    }
  }
  return total
}

function evictIfOverCap(): void {
  let total = totalStoredBytes()
  if (total <= MAX_TOTAL_DRAFT_BYTES) return

  // Gather all items sorted by addedAt ascending (oldest first)
  const all: { botId: string; item: DraftAttachmentItem }[] = []
  for (const [botId, list] of memoryDrafts.entries()) {
    for (const item of list) {
      all.push({ botId, item })
    }
  }
  all.sort((a, b) => a.item.addedAt - b.item.addedAt)

  const toDeleteIds: string[] = []
  for (const entry of all) {
    if (total <= MAX_TOTAL_DRAFT_BYTES) break
    const list = memoryDrafts.get(entry.botId)
    if (list) {
      const idx = list.findIndex((x) => x.key === entry.item.key)
      if (idx !== -1) {
        list.splice(idx, 1)
        total -= entry.item.size || 0
        toDeleteIds.push(`${entry.botId}:${entry.item.key}`)
      }
      if (list.length === 0) memoryDrafts.delete(entry.botId)
    }
  }
  if (toDeleteIds.length > 0) {
    void deleteRows(toDeleteIds)
  }
}

/** Get current drafts for a bot from memory synchronously. */
export function getAttachmentDrafts(botId: string): DraftAttachmentItem[] {
  return memoryDrafts.get(botId) ? [...memoryDrafts.get(botId)!] : []
}

/** Save a new attachment draft item for a bot. */
export function saveAttachmentDraft(botId: string, item: DraftAttachmentItem): void {
  let list = memoryDrafts.get(botId)
  if (!list) {
    list = []
    memoryDrafts.set(botId, list)
  }
  const existingIdx = list.findIndex((it) => it.key === item.key)
  if (existingIdx !== -1) {
    list[existingIdx] = item
  } else {
    // If bot has reached item cap, evict oldest for this bot
    if (list.length >= MAX_ITEMS_PER_BOT) {
      const evicted = list.shift()
      if (evicted) {
        void deleteRow(`${botId}:${evicted.key}`)
      }
    }
    list.push(item)
  }

  const row: DraftAttachmentRow = {
    id: `${botId}:${item.key}`,
    botId,
    key: item.key,
    fp: item.fp,
    name: item.name,
    size: item.size,
    originalSize: item.originalSize,
    isImage: item.isImage,
    attachmentId: item.id,
    blob: item.file,
    addedAt: item.addedAt,
  }
  void putRow(row)
  evictIfOverCap()
}

/** Update the daemon attachment id once an upload finishes. */
export function updateAttachmentDraftId(botId: string, key: string, attachmentId: string): void {
  const list = memoryDrafts.get(botId)
  if (!list) return
  const item = list.find((it) => it.key === key)
  if (!item) return
  item.id = attachmentId

  const row: DraftAttachmentRow = {
    id: `${botId}:${item.key}`,
    botId,
    key: item.key,
    fp: item.fp,
    name: item.name,
    size: item.size,
    originalSize: item.originalSize,
    isImage: item.isImage,
    attachmentId,
    blob: item.file,
    addedAt: item.addedAt,
  }
  void putRow(row)
}

/** Remove a single attachment draft item. */
export function removeAttachmentDraft(botId: string, key: string): void {
  const list = memoryDrafts.get(botId)
  if (!list) return
  const idx = list.findIndex((it) => it.key === key)
  if (idx !== -1) {
    list.splice(idx, 1)
    if (list.length === 0) memoryDrafts.delete(botId)
    void deleteRow(`${botId}:${key}`)
  }
}

/** Remove attachments that were sent. */
export function removeSentAttachmentDrafts(botId: string, sentIds: readonly string[]): void {
  const list = memoryDrafts.get(botId)
  if (!list || list.length === 0) return
  const sent = new Set(sentIds)
  const toDeleteIds: string[] = []
  const remaining = list.filter((it) => {
    if (it.id && sent.has(it.id)) {
      toDeleteIds.push(`${botId}:${it.key}`)
      return false
    }
    return true
  })
  if (remaining.length === 0) {
    memoryDrafts.delete(botId)
  } else {
    memoryDrafts.set(botId, remaining)
  }
  if (toDeleteIds.length > 0) {
    void deleteRows(toDeleteIds)
  }
}

/** Clear all draft attachments for a bot (on explicit clear or prompt send without remaining attachments). */
export function clearAttachmentDrafts(botId: string): void {
  const list = memoryDrafts.get(botId)
  if (!list || list.length === 0) return
  const toDeleteIds = list.map((it) => `${botId}:${it.key}`)
  memoryDrafts.delete(botId)
  void deleteRows(toDeleteIds)
}

/** Notify callback once initial IndexedDB read completes. */
export function onAttachmentDraftsLoaded(cb: () => void): () => void {
  if (dbLoaded) {
    cb()
    return () => {}
  }
  loadListeners.add(cb)
  return () => {
    loadListeners.delete(cb)
  }
}

let started = false
let sweepTimer: ReturnType<typeof setInterval> | null = null

export function sweepExpiredAttachmentDrafts(): void {
  const cutoff = Date.now() - DRAFT_ATTACHMENT_TTL_MS
  const toDeleteIds: string[] = []
  for (const [botId, list] of memoryDrafts.entries()) {
    const remaining = list.filter((it) => {
      if (it.addedAt < cutoff) {
        toDeleteIds.push(`${botId}:${it.key}`)
        return false
      }
      return true
    })
    if (remaining.length === 0) {
      memoryDrafts.delete(botId)
    } else {
      memoryDrafts.set(botId, remaining)
    }
  }
  if (toDeleteIds.length > 0) {
    void deleteRows(toDeleteIds)
  }
}

export function startAttachmentDraftPersistence(): () => void {
  if (started) return () => {}
  started = true

  void readAllRows().then((rows) => {
    const cutoff = Date.now() - DRAFT_ATTACHMENT_TTL_MS
    const live = rows.filter((r) => r.addedAt >= cutoff)
    const dead = rows.filter((r) => r.addedAt < cutoff).map((r) => r.id)
    if (dead.length) void deleteRows(dead)

    for (const r of live) {
      let list = memoryDrafts.get(r.botId)
      if (!list) {
        list = []
        memoryDrafts.set(r.botId, list)
      }
      if (!list.some((it) => it.key === r.key)) {
        const item: DraftAttachmentItem = {
          key: r.key,
          fp: r.fp,
          name: r.name,
          size: r.size,
          originalSize: r.originalSize,
          isImage: r.isImage,
          id: r.attachmentId,
          file: r.blob ? new File([r.blob], r.name, { type: r.blob.type }) : undefined,
          addedAt: r.addedAt,
        }
        list.push(item)
      }
    }
    evictIfOverCap()
    dbLoaded = true
    for (const cb of loadListeners) cb()
    loadListeners.clear()
  })

  if (sweepTimer) clearInterval(sweepTimer)
  sweepTimer = setInterval(sweepExpiredAttachmentDrafts, SWEEP_INTERVAL_MS)

  return () => {
    if (sweepTimer) {
      clearInterval(sweepTimer)
      sweepTimer = null
    }
    started = false
  }
}

/** Reset in-memory state and clear timers for test isolation. */
export function resetAttachmentDraftsForTest(): void {
  memoryDrafts.clear()
  loadListeners.clear()
  if (sweepTimer) {
    clearInterval(sweepTimer)
    sweepTimer = null
  }
  started = false
  dbLoaded = false
}
