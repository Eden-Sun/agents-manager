/**
 * 群組輸入區的附件失敗（issue #917，真的掛 `GroupChatPanel` 進 happy-dom，後端是 `MockTransport`）：
 * 上傳失敗的卡片不在 `files.ids` 裡，照送會少一個檔案、送成功後還把失敗卡片連重試鈕一起清掉——所以要擋送、提示、保留卡片。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, keydown, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { ApiError } from '../api/types'
import { ATTACHMENT_FAILED_NOTICE } from '../lib/composerLabels'
import { groupComposerState, resetStoreForTest, useStore } from '../store/store'
import { GroupChatPanel } from './GroupChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!
const chats = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/projects\/[^/]+\/chat/.test(r.path))

async function dropFile(name: string) {
  const input = document.querySelector<HTMLInputElement>('input[type=file]')!
  const file = new File([new Uint8Array([137, 80, 78, 68])], name, { type: 'image/png' })
  Object.defineProperty(input, 'files', { value: [file], configurable: true })
  await act(async () => { input.dispatchEvent(new Event('change', { bubbles: true })) })
}

function failNextUpload(): () => void {
  const original = mock.upload.bind(mock)
  let armed = true
  mock.upload = (async (path: string, file: Blob, opts?: Parameters<typeof original>[2]) => {
    if (armed) {
      armed = false
      throw new ApiError(502, { error: 'upstream', message: 'mock: 寫入附件失敗' }, 'POST attachments failed (502)')
    }
    return original(path, file, opts)
  }) as typeof mock.upload
  return () => {
    mock.upload = original as typeof mock.upload
  }
}

test('#917 群組輸入區：附件上傳失敗後 Enter／送出鈕不送出、卡片與重試還在；重試成功後才帶該 id 送出', { timeout: 30_000 }, async () => {
  const restore = failNextUpload()
  try {
    const requests = mockApi(mock)
    // 自己的專案與 bot：同一個行程裡別的測試檔會讓內建專案的 bot 一直在跑回合（群組輸入區因此鎖住）。
    const { project_id: projectId } = (await mock.request('POST', '/projects', { path: '/tmp/group-attach-fail', label: 'group-attach-fail' })) as { project_id: string }
    const created = (await mock.request('POST', `/projects/${projectId}/bots`, { name: 'gaf-claude', kind: 'claude' })) as { id?: string; bot_id?: string }
    await useStore.getState().refreshState()
    const bot = useStore.getState().bots.find((b) => b.name === 'gaf-claude')!
    assert.ok(bot, `建好 bot（${JSON.stringify(created)}）`)
    await mock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running' && !groupComposerState(useStore.getState(), projectId).disabled
    }, 'gaf-claude running 且群組輸入區可送出')
    await mount(<GroupChatPanel projectId={projectId} onOpenSidebar={() => {}} />)
    await settle(200)

    const input = document.querySelector<HTMLInputElement>('input[type=file]')!
    const file = new File([new Uint8Array([137, 80, 78, 71])], 'shot.png', { type: 'image/png' })
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    await act(async () => {
      input.dispatchEvent(new Event('change', { bubbles: true }))
    })
    await until(() => document.querySelectorAll('.attach-thumb.failed').length === 1, '上傳失敗的卡片')
    const retryBtn = () => [...document.querySelectorAll('.attach-thumb button')].find((b) => b.textContent?.includes('重試'))
    assert.ok(retryBtn(), '失敗卡片有「重試」')

    await typeInto(textarea(), '@gaf-claude with a file')
    const sendBtn = document.querySelector<HTMLButtonElement>('.send-btn')!
    assert.equal(sendBtn.disabled, true, '有附件失敗時送出鈕 disabled')
    assert.match(sendBtn.title, /附件上傳失敗/)

    await keydown(textarea(), 'Enter')
    await settle(200)
    assert.equal(chats(requests).length, 0, 'Enter 沒有送出群組訊息')
    assert.equal(textarea().value, '@gaf-claude with a file', '字還在輸入框')
    assert.equal(document.querySelectorAll('.attach-thumb.failed').length, 1, '失敗卡片還在')
    assert.ok(retryBtn(), '「重試」還在')
    assert.ok(useStore.getState().notices.some((n) => n.kind === 'error' && n.text === ATTACHMENT_FAILED_NOTICE), '出現錯誤通知')

    await click(retryBtn()!)
    await until(() => document.querySelectorAll('.attach-thumb:not(.uploading):not(.failed)').length === 1, '重試成功')
    await keydown(textarea(), 'Enter')
    await until(() => chats(requests).length === 1, '重試成功後送出')
    const body = chats(requests)[0].body as { text: string; attachments?: string[] }
    assert.equal(body.attachments?.length, 1, '群組訊息帶了該附件 id')
  } finally {
    restore()
  }
})

test('交給 AGM 模式：打 @ 不彈候選，Enter 直接建任務、文字不被改寫（#1134）', { timeout: 30_000 }, async () => {
  const requests = mockApi(mock)
  const { project_id: projectId } = (await mock.request('POST', '/projects', { path: '/tmp/group-agm-at', label: 'group-agm-at' })) as { project_id: string }
  await mock.request('POST', `/projects/${projectId}/bots`, { name: 'gag-claude', kind: 'claude' })
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === 'gag-claude')!
  await mock.request('POST', `/bots/${bot.id}/start`)
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.state === 'running' && !groupComposerState(useStore.getState(), projectId).disabled
  }, 'gag-claude running 且群組輸入區可送出')
  await mount(<GroupChatPanel projectId={projectId} onOpenSidebar={() => {}} />)
  await settle(200)

  await click(document.querySelector<HTMLElement>('.agm-toggle')!)
  await settle(50)
  await typeInto(textarea(), '請看 @')
  assert.equal(document.querySelector('.mention-pop'), null, '交給 AGM 時不彈 @ 候選')
  assert.notEqual(textarea().getAttribute('aria-expanded'), 'true')
  await keydown(textarea(), 'Enter')
  await until(() => requests.some((r) => r.method === 'POST' && /\/projects\/[^/]+\/missions/.test(r.path)), '建了任務')
  const created = requests.find((r) => r.method === 'POST' && /\/projects\/[^/]+\/missions/.test(r.path))!
  assert.equal((created.body as { text: string }).text, '請看 @', '任務說明的文字沒被改寫')
  assert.equal(chats(requests).length, 0, '沒有走一般的群組訊息')

  // 對照組：沒開「交給 AGM」時打 @ 照樣彈候選。
  await click(document.querySelector<HTMLElement>('.agm-toggle')!)
  await settle(50)
  await typeInto(textarea(), '@')
  assert.ok(document.querySelector('.mention-pop'), '一般模式打 @ 要彈候選')
})

test('群組輸入框：Office 類複製的文字不變成附件；只有 Files 的貼上仍收附件', { timeout: 30_000 }, async () => {
  const requests = mockApi(mock)
  const { project_id: projectId } = (await mock.request('POST', '/projects', { path: '/tmp/group-office-paste', label: 'group-office-paste' })) as { project_id: string }
  await mock.request('POST', `/projects/${projectId}/bots`, { name: 'gop-claude', kind: 'claude' })
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === 'gop-claude')!
  await mock.request('POST', `/bots/${bot.id}/start`)
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.state === 'running' && !groupComposerState(useStore.getState(), projectId).disabled
  }, 'gop-claude running 且群組輸入區可送出')
  await mount(<GroupChatPanel projectId={projectId} onOpenSidebar={() => {}} />)
  await settle(200)

  const image = new File([new Uint8Array([1])], 'image.png', { type: 'image/png' })
  const paste = async (clipboardData: { files: File[]; types: string[]; getData(type: string): string }) => {
    const ev = new Event('paste', { bubbles: true, cancelable: true })
    Object.defineProperty(ev, 'clipboardData', { value: clipboardData })
    await act(async () => { textarea().dispatchEvent(ev) })
    return ev
  }
  const office = await paste({ files: [image], types: ['text/plain', 'text/html', 'text/rtf', 'Files'], getData: (type) => type === 'text/plain' ? 'A\tB' : '' })
  assert.equal(office.defaultPrevented, false)
  assert.equal(document.querySelector('.attach-tray'), null)
  assert.equal(requests.some((r) => r.method === 'POST' && /\/attachments/.test(r.path)), false)

  const filePaste = await paste({ files: [image], types: ['Files'], getData: () => '' })
  assert.equal(filePaste.defaultPrevented, true)
  assert.equal(document.querySelectorAll('.attach-tray .attach-thumb').length, 1)
})

test('群組：送出期間加進來的附件留在托盤', { timeout: 30_000 }, async () => {
  const requests = mockApi(mock)
  const { project_id: projectId } = (await mock.request('POST', '/projects', { path: '/tmp/group-late-attachment', label: 'group-late-attachment' })) as { project_id: string }
  await mock.request('POST', `/projects/${projectId}/bots`, { name: 'gla-claude', kind: 'claude' })
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === 'gla-claude')!
  await mock.request('POST', `/bots/${bot.id}/start`)
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.state === 'running' && !groupComposerState(useStore.getState(), projectId).disabled
  }, 'gla-claude running 且群組輸入區可送出')
  await mount(<GroupChatPanel projectId={projectId} onOpenSidebar={() => {}} />)
  await settle(200)

  const originalFetch = globalThis.fetch
  let releaseChat!: () => void
  const chatGate = new Promise<void>((resolve) => { releaseChat = resolve })
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    if (init?.method === 'POST' && /\/projects\/[^/]+\/chat/.test(String(input))) await chatGate
    return originalFetch(input, init)
  }) as typeof fetch
  try {
    await typeInto(textarea(), '@all hi')
    await keydown(textarea(), 'Enter')
    await until(() => textarea().disabled, '群組訊息送出中')
    await dropFile('late.png')
    await until(() => document.querySelector('.attach-thumb:not(.uploading)') !== null, '送出期間加入的附件上傳完成')
    releaseChat()
    await until(() => chats(requests).length === 1, '第一則群組訊息完成')
    await settle()
    assert.equal(textarea().value, '')
    assert.equal(document.querySelectorAll('.attach-tray .attach-thumb').length, 1)
    const first = chats(requests)[0].body as { attachments?: string[] }
    assert.equal(first.attachments, undefined, '送出期間新加的檔案不會帶進第一則')
  } finally {
    releaseChat()
    globalThis.fetch = originalFetch
  }
})
