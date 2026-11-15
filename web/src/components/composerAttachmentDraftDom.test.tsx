import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { IDBFactory } from 'fake-indexeddb'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { useAttachments } from './attachmentsHelpers'
import { AttachTray } from './Attachments'
import {
  resetAttachmentDraftsForTest,
  startAttachmentDraftPersistence,
  DRAFT_ATTACHMENT_DB_NAME,
} from '../store/composerAttachmentDrafts'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  resetAttachmentDraftsForTest()
  await teardownDom()
})

let factory: IDBFactory
let savedIdb: unknown
let hadIdb: boolean

test.beforeEach(() => {
  factory = new IDBFactory()
  hadIdb = 'indexedDB' in globalThis
  savedIdb = (globalThis as { indexedDB?: unknown }).indexedDB
  Object.assign(globalThis, { indexedDB: factory })
  resetAttachmentDraftsForTest()
})

test.afterEach(() => {
  resetAttachmentDraftsForTest()
  if (hadIdb) Object.assign(globalThis, { indexedDB: savedIdb })
  else Reflect.deleteProperty(globalThis, 'indexedDB')
})

test('切換 bot：bot1 夾帶附件後切到 bot2，再切回 bot1 附件依然存在且保持已上傳 id', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  let currentAttachments!: ReturnType<typeof useAttachments>
  let setBot!: (b: string) => void

  function MultiBotFixture() {
    const [bot, setB] = useState('bot1')
    setBot = setB
    currentAttachments = useAttachments(bot, bot)
    return <AttachTray items={currentAttachments.items} onRemove={currentAttachments.remove} onRetry={currentAttachments.retry} />
  }

  await mount(<MultiBotFixture />)

  // 1. 在 bot1 加入圖片與 PDF
  const imgFile = new File([new Uint8Array([1, 2, 3])], 'photo.png', { type: 'image/png', lastModified: 100 })
  await act(async () => currentAttachments.add([imgFile]))
  await until(() => currentAttachments.items.length === 1 && Boolean(currentAttachments.items[0].id), 'bot1 附件上傳完成並取得 id')
  const bot1AttId = currentAttachments.items[0].id!
  assert.ok(bot1AttId)

  // 2. 切換至 bot2
  await act(async () => setBot('bot2'))
  await until(() => currentAttachments.items.length === 0, '切到 bot2 時托盤為空')

  // 在 bot2 放入另外一份附件
  const bot2Doc = new File([new Uint8Array([4, 5])], 'doc2.pdf', { type: 'application/pdf', lastModified: 200 })
  await act(async () => currentAttachments.add([bot2Doc]))
  await until(() => currentAttachments.items.length === 1 && Boolean(currentAttachments.items[0].id), 'bot2 附件上傳完成')
  assert.equal(currentAttachments.items[0].name, 'doc2.pdf')

  // 3. 切回 bot1：bot1 原本的附件完整還原！
  await act(async () => setBot('bot1'))
  await until(() => currentAttachments.items.length === 1 && currentAttachments.items[0].name === 'photo.png', '切回 bot1 還原原本附件')
  assert.equal(currentAttachments.items[0].id, bot1AttId)
  assert.equal(currentAttachments.items[0].isImage, true)

  // 4. 再切回 bot2：bot2 的附件也還原！
  await act(async () => setBot('bot2'))
  await until(() => currentAttachments.items.length === 1 && currentAttachments.items[0].name === 'doc2.pdf', '切回 bot2 還原 doc2')
})

test('重新整理／重啟：從 IndexedDB 還原未送出的附件草稿', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  const now = Date.now()

  // 1. 事先將之前對話留下的附件寫進 IndexedDB（模擬上一分頁或關閉前的暫存）
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
        id: 'bot_reload:a1',
        botId: 'bot_reload',
        key: 'a1',
        fp: 'persisted.pdf|1024|1',
        name: 'persisted.pdf',
        size: 1024,
        isImage: false,
        attachmentId: 'att_persisted_id',
        blob: new Blob(['pdf-bytes'], { type: 'application/pdf' }),
        addedAt: now,
      })
      t.oncomplete = () => resolve()
      t.onerror = () => reject(t.error)
    }
  })

  // 2. 啟動草稿持久化
  const stop = startAttachmentDraftPersistence()

  let attachments!: ReturnType<typeof useAttachments>
  function ReloadFixture() {
    attachments = useAttachments('bot_reload', 'bot_reload')
    return <AttachTray items={attachments.items} onRemove={attachments.remove} onRetry={attachments.retry} />
  }

  await mount(<ReloadFixture />)

  // 3. 驗證從 IndexedDB 自動載入並還原至 DOM
  await until(
    () => attachments.items.length === 1 && attachments.items[0].name === 'persisted.pdf' && attachments.items[0].id === 'att_persisted_id',
    '從 IndexedDB 還原附件'
  )
  assert.equal(attachments.items[0].id, 'att_persisted_id')
  assert.equal(attachments.items[0].size, 1024)

  stop()
})

test('清空草稿與送出成功：分別正確清除暫存', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  let attachments!: ReturnType<typeof useAttachments>
  function Fixture() {
    attachments = useAttachments('bot_clear', 'bot_clear')
    return <AttachTray items={attachments.items} onRemove={attachments.remove} onRetry={attachments.retry} />
  }

  await mount(<Fixture />)
  const f1 = new File([new Uint8Array([1])], 'file1.txt', { type: 'text/plain', lastModified: 10 })
  const f2 = new File([new Uint8Array([2])], 'file2.txt', { type: 'text/plain', lastModified: 20 })
  await act(async () => attachments.add([f1, f2]))
  await until(() => attachments.items.length === 2 && attachments.items.every((it) => it.id), '兩張附件均上傳完畢')

  const id1 = attachments.items[0].id!
  const id2 = attachments.items[1].id!

  // 送出時 clearSent 指定 id1
  await act(async () => attachments.clearSent([id1]))
  assert.equal(attachments.items.length, 1)
  assert.equal(attachments.items[0].id, id2)

  // 清空 composer
  await act(async () => attachments.clear())
  assert.equal(attachments.items.length, 0)
})
