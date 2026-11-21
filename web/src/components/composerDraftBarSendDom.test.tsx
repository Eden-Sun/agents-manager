import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { flush } from '../testing/flush'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const clearButton = () => [...document.querySelectorAll<HTMLButtonElement>('.composer-draft button')].find((b) => b.textContent?.includes('清掉再送'))!
const draftPuts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'PUT' && r.path.includes('/drafts/'))
const emptyPuts = (requests: FakeRequest[]) => draftPuts(requests).filter((r) => (r.body as { text?: string } | undefined)?.text === '')

function gate<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

async function remoteDraft(key: string, text: string) {
  await act(async () => useStore.setState((s) => ({ drafts: { ...s.drafts, [key]: text } })))
}

async function openStuck(name: string, draft: string) {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === name)!
  assert.ok(bot, `mock 要有 ${name}`)
  if (useStore.getState().runs[bot.id]?.state !== 'running') {
    await mock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running'
    }, `${name} running`)
  }
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await flush()
  await useStore.getState().setDraft(`bot:${bot.id}`, draft)
  const input = document.querySelector<HTMLInputElement>('input[type=file]')!
  const file = new File([new Uint8Array([137, 80, 78, 71])], 'shot.png', { type: 'image/png' })
  Object.defineProperty(input, 'files', { value: [file], configurable: true })
  await act(async () => {
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await until(() => document.querySelectorAll('.attach-thumb:not(.uploading)').length === 1, '附件上傳完')
  useStore.setState((s) => ({
    composerDrafts: { ...s.composerDrafts, [bot.id]: { draft: 'stuck', token: 't1', truncated: false, actions: ['submit', 'clear'] } },
  }) as never)
  await until(() => clearButton() !== undefined, '清掉再送的按鈕出現')
  return { requests, bot }
}

test('清掉再送：送出途中本機打字 → 成功後文字保留、附件照清', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openStuck('am-claude', 'mine text')
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await flush()
  await act(async () => clearButton().click())
  await act(async () => useStore.getState().setDraft(`bot:${bot.id}`, 'typed locally'))
  await act(async () => sent.resolve(true))
  await flush()
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`], 'typed locally', '本機打字還在')
  assert.equal(document.querySelectorAll('.attach-thumb').length, 0, '附件照清')
  assert.deepEqual(emptyPuts(requests), [])
})

test('清掉再送：送出途中另一裝置同步來新草稿 → 文字保留、附件照清', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openStuck('am-claude-2', 'mine text')
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await flush()
  await act(async () => clearButton().click())
  await remoteDraft(`bot:${bot.id}`, 'new text from another device')
  await act(async () => sent.resolve(true))
  await flush()
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`], 'new text from another device', '新草稿還在')
  assert.equal(document.querySelectorAll('.attach-thumb').length, 0, '附件照清')
  assert.deepEqual(emptyPuts(requests), [])
})

test('清掉再送：草稿沒被改過 → 照常清空文字與附件', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openStuck('am-codex', 'mine text')
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await flush()
  await act(async () => clearButton().click())
  await act(async () => sent.resolve(true))
  await flush()
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`] ?? '', '')
  assert.equal(document.querySelectorAll('.attach-thumb').length, 0)
  assert.equal(emptyPuts(requests).length, 1, '清空要同步回 daemon')
})

test('清掉再送：送出失敗（resolve(false)）不清文字與附件，訂閱解除', { timeout: 30_000 }, async () => {
  const { bot } = await openStuck('am-claude', 'mine text')
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await flush()
  await act(async () => clearButton().click())
  await act(async () => sent.resolve(false))
  await flush()
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`], 'mine text', '失敗後文字照舊')
  assert.equal(document.querySelectorAll('.attach-thumb').length, 1, '失敗後附件還在')
})

test('清掉再送：送出連線失敗（傳輸層 reject）不清文字與附件，訂閱解除', { timeout: 30_000 }, async () => {
  const { bot } = await openStuck('am-codex', 'mine text')
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    if (init?.method === 'POST' && /\/prompt/.test(String(input))) throw new TypeError('network down')
    return realFetch(input, init as RequestInit)
  }) as typeof fetch
  try {
    await act(async () => clearButton().click())
    await flush()
    assert.equal(useStore.getState().drafts[`bot:${bot.id}`], 'mine text', '連線失敗後文字照舊')
    assert.equal(document.querySelectorAll('.attach-thumb').length, 1, '連線失敗後附件還在')
  } finally {
    globalThis.fetch = realFetch
  }
})
