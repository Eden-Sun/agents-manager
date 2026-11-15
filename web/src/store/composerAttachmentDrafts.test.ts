import test, { afterEach, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { IDBFactory } from 'fake-indexeddb'
import {
  clearAttachmentDrafts,
  getAttachmentDrafts,
  removeAttachmentDraft,
  removeSentAttachmentDrafts,
  resetAttachmentDraftsForTest,
  saveAttachmentDraft,
  startAttachmentDraftPersistence,
  sweepExpiredAttachmentDrafts,
  updateAttachmentDraftId,
  DRAFT_ATTACHMENT_DB_NAME,
  DRAFT_ATTACHMENT_TTL_MS,
  MAX_ITEMS_PER_BOT,
} from './composerAttachmentDrafts'

let factory: IDBFactory
let savedIdb: unknown
let hadIdb: boolean

beforeEach(() => {
  factory = new IDBFactory()
  hadIdb = 'indexedDB' in globalThis
  savedIdb = (globalThis as { indexedDB?: unknown }).indexedDB
  Object.assign(globalThis, { indexedDB: factory })
  resetAttachmentDraftsForTest()
})

afterEach(() => {
  resetAttachmentDraftsForTest()
  if (hadIdb) Object.assign(globalThis, { indexedDB: savedIdb })
  else Reflect.deleteProperty(globalThis, 'indexedDB')
})

test('saveAttachmentDraft 存入記憶體並可被 getAttachmentDrafts 讀取', () => {
  const file = new File(['test content'], 'test.txt', { type: 'text/plain' })
  saveAttachmentDraft('bot1', {
    key: 'a1',
    fp: 'test.txt|12|123',
    name: 'test.txt',
    size: 12,
    isImage: false,
    id: null,
    file,
    addedAt: Date.now(),
  })

  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts.length, 1)
  assert.equal(drafts[0].name, 'test.txt')
  assert.equal(drafts[0].id, null)
})

test('updateAttachmentDraftId 更新已上傳附件的 id', () => {
  saveAttachmentDraft('bot1', {
    key: 'a1',
    fp: 'photo.png|100|1',
    name: 'photo.png',
    size: 100,
    isImage: true,
    id: null,
    addedAt: Date.now(),
  })

  updateAttachmentDraftId('bot1', 'a1', 'att_12345')
  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts[0].id, 'att_12345')
})

test('removeAttachmentDraft 移除單一卡片', () => {
  saveAttachmentDraft('bot1', {
    key: 'a1',
    fp: '1',
    name: '1.txt',
    size: 10,
    isImage: false,
    id: null,
    addedAt: Date.now(),
  })
  saveAttachmentDraft('bot1', {
    key: 'a2',
    fp: '2',
    name: '2.txt',
    size: 20,
    isImage: false,
    id: null,
    addedAt: Date.now(),
  })

  removeAttachmentDraft('bot1', 'a1')
  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts.length, 1)
  assert.equal(drafts[0].key, 'a2')
})

test('removeSentAttachmentDrafts 送出後只移除已送出的附件，保留其他附件', () => {
  saveAttachmentDraft('bot1', {
    key: 'a1',
    fp: '1',
    name: '1.txt',
    size: 10,
    isImage: false,
    id: 'id_1',
    addedAt: Date.now(),
  })
  saveAttachmentDraft('bot1', {
    key: 'a2',
    fp: '2',
    name: '2.txt',
    size: 20,
    isImage: false,
    id: 'id_2',
    addedAt: Date.now(),
  })

  removeSentAttachmentDrafts('bot1', ['id_1'])
  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts.length, 1)
  assert.equal(drafts[0].id, 'id_2')
})

test('clearAttachmentDrafts 清空該 bot 的所有草稿附件', () => {
  saveAttachmentDraft('bot1', {
    key: 'a1',
    fp: '1',
    name: '1.txt',
    size: 10,
    isImage: false,
    id: 'id_1',
    addedAt: Date.now(),
  })
  clearAttachmentDrafts('bot1')
  assert.equal(getAttachmentDrafts('bot1').length, 0)
})

test('超過 MAX_ITEMS_PER_BOT 時自動淘汰該 bot 最舊的草稿', () => {
  for (let i = 0; i < MAX_ITEMS_PER_BOT + 2; i++) {
    saveAttachmentDraft('bot1', {
      key: `a${i}`,
      fp: `fp_${i}`,
      name: `${i}.txt`,
      size: 10,
      isImage: false,
      id: `id_${i}`,
      addedAt: 1000 + i,
    })
  }

  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts.length, MAX_ITEMS_PER_BOT)
  // 最早的 a0, a1 應已被淘汰，最早保留的是 a2
  assert.equal(drafts[0].key, 'a2')
})

test('sweepExpiredAttachmentDrafts 清除超過 24 小時的過期草稿', () => {
  const now = Date.now()
  saveAttachmentDraft('bot1', {
    key: 'old',
    fp: 'old',
    name: 'old.txt',
    size: 10,
    isImage: false,
    id: 'old_id',
    addedAt: now - DRAFT_ATTACHMENT_TTL_MS - 1000,
  })
  saveAttachmentDraft('bot1', {
    key: 'new',
    fp: 'new',
    name: 'new.txt',
    size: 10,
    isImage: false,
    id: 'new_id',
    addedAt: now,
  })

  sweepExpiredAttachmentDrafts()
  const drafts = getAttachmentDrafts('bot1')
  assert.equal(drafts.length, 1)
  assert.equal(drafts[0].key, 'new')
})

test('startAttachmentDraftPersistence 啟動時從 IndexedDB 還原並淘汰過期檔案', async () => {
  // 1. 寫入假 DB
  const now = Date.now()
  const dbReq = factory.open(DRAFT_ATTACHMENT_DB_NAME, 1)
  await new Promise<void>((resolve, reject) => {
    dbReq.onupgradeneeded = () => {
      const store = dbReq.result.createObjectStore('drafts', { keyPath: 'id' })
      store.createIndex('botId', 'botId', { unique: false })
      store.createIndex('addedAt', 'addedAt', { unique: false })
    }
    dbReq.onsuccess = () => {
      const db = dbReq.result
      const t = db.transaction('drafts', 'readwrite')
      const store = t.objectStore('drafts')
      store.put({
        id: 'bot_idb:k1',
        botId: 'bot_idb',
        key: 'k1',
        fp: 'k1_fp',
        name: 'restored.pdf',
        size: 500,
        isImage: false,
        attachmentId: 'att_restored',
        addedAt: now,
      })
      store.put({
        id: 'bot_idb:k_expired',
        botId: 'bot_idb',
        key: 'k_expired',
        fp: 'k_exp',
        name: 'expired.pdf',
        size: 500,
        isImage: false,
        attachmentId: 'att_exp',
        addedAt: now - DRAFT_ATTACHMENT_TTL_MS - 5000,
      })
      t.oncomplete = () => resolve()
      t.onerror = () => reject(t.error)
    }
  })

  // 2. 啟動 persistence
  const stop = startAttachmentDraftPersistence()
  try {
    // 等候非同步讀取完成
    for (let i = 0; i < 20; i++) {
      if (getAttachmentDrafts('bot_idb').length > 0) break
      await new Promise((r) => setTimeout(r, 10))
    }
    const drafts = getAttachmentDrafts('bot_idb')
    assert.equal(drafts.length, 1)
    assert.equal(drafts[0].name, 'restored.pdf')
    assert.equal(drafts[0].id, 'att_restored')
  } finally {
    stop()
  }
})
