/**
 * 送出途中草稿被別的裝置改過（#1811，真的掛 `ChatPanel`／`GroupChatPanel` 進 happy-dom，後端是 `fakeApi`）：
 * 草稿跨裝置共用，送出請求在路上時 WS 進來的新草稿會套到本機；舊的送出成功回來，只有草稿沒被改過才能清框，
 * 否則新草稿會被清掉，空字串還會同步回 daemon。四條送出路徑：一般、send-now（插隊）、併送（補充）、群組。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { groupComposerState, resetStoreForTest, useStore } from '../store/store'
import { toRun } from '../api/normalize'
import { ChatPanel } from './ChatPanel'
import { GroupChatPanel } from './GroupChatPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
after(teardownDom)

const NEW = 'new text from another device'
const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!
const button = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('.composer button')].find((b) => b.textContent === label)!
/** 同步到草稿 key 的 PUT（空字串＝送出清框後同步回 daemon）。 */
const draftPuts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'PUT' && r.path.includes('/drafts/'))
const emptyPuts = (requests: FakeRequest[]) => draftPuts(requests).filter((r) => (r.body as { text?: string } | undefined)?.text === '')

/** 送出函式手動放行：`resolve` 之前 `await` 的送出都還在途中。 */
function gate<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

/** 另一台裝置經 WS 把新草稿套進來（同 `draftSync` 的 `write`：直接改 store，不再觸發本機同步）。 */
async function remoteDraft(key: string, text: string) {
  await act(async () => useStore.setState((s) => ({ drafts: { ...s.drafts, [key]: text } })))
}

function seedBot(draft: string, inFlight: boolean) {
  useStore.setState({
    connected: true,
    defaultConnected: true,
    selectedBotId: 'b',
    bots: [{ id: 'b', name: 'b', kind: 'claude', project_id: 'p', herdr_session: 'agents-manager', managed_by: 'user', parent_bot_id: null, args: [], env: {} }],
    projects: [{ id: 'p', path: '/p', host: 'local' }],
    runs: { b: toRun({ id: 'r', bot_id: 'b', state: 'running', agent_status: inFlight ? 'working' : 'idle', send_now_supported: true }) },
    turns: inFlight ? { b: { t: { id: 't', run_id: 'r', status: 'in_flight', delivery: 'ok' } } } : { b: {} },
    drafts: { 'bot:b': draft },
    messages: { b: [] },
  } as never)
}

function seedGroup(draft: string) {
  useStore.setState({
    connected: true,
    defaultConnected: true,
    bots: [{ id: 'gb', name: 'gb', kind: 'claude', project_id: 'gp', herdr_session: 'agents-manager', managed_by: 'user', parent_bot_id: null, args: [], env: {} }],
    projects: [{ id: 'gp', path: '/gp', host: 'local', label: 'gp' }],
    runs: { gb: toRun({ id: 'gr', bot_id: 'gb', state: 'running', agent_status: 'idle' }) },
    turns: { gb: {} },
    loadedProjects: { gp: true },
    drafts: { 'group:gp': draft },
    groupMessages: { gp: [] },
    messages: { gb: [] },
  } as never)
  assert.equal(groupComposerState(useStore.getState(), 'gp').disabled, false, '群組輸入區可送出')
}

test('一般送出：送出途中別的裝置送來新草稿，成功回來後保留新草稿、不同步空字串', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await keydown(textarea(), 'Enter')
  await remoteDraft('bot:b', NEW)
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], NEW, '新草稿還在')
  assert.deepEqual(emptyPuts(requests), [], '沒有把空字串同步回 daemon')
})

test('一般送出：送出途中草稿沒被改過，成功後照常清空並同步空字串', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await keydown(textarea(), 'Enter')
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'] ?? '', '')
  assert.equal(emptyPuts(requests).length, 1, '清空要同步回 daemon')
})

test('一般送出：送出途中草稿被改成別的字又改回同一句，成功後也不清（算改過一輪）', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await keydown(textarea(), 'Enter')
  await remoteDraft('bot:b', NEW)
  await remoteDraft('bot:b', 'first prompt')
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], 'first prompt', '同文字的新一輪草稿還在')
})

test('send-now（插隊）：送出途中草稿被改過就保留，沒改過才清', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedBot('first prompt', true)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await act(async () => button('插隊').click())
  await remoteDraft('bot:b', NEW)
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], NEW, 'send-now 成功後新草稿還在')
  assert.deepEqual(emptyPuts(requests), [])
})

test('send-now（插隊）：草稿沒被改過，成功後照常清空', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', true)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await act(async () => button('插隊').click())
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'] ?? '', '')
})

test('併送（補充）：送出途中草稿被改過就保留，沒改過才清', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', true)
  const sent = gate<boolean>()
  useStore.setState({ sendText: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await act(async () => button('補充').click())
  await remoteDraft('bot:b', NEW)
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], NEW, '併送成功後新草稿還在')
})

test('併送（補充）：草稿沒被改過，成功後照常清空', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', true)
  const sent = gate<boolean>()
  useStore.setState({ sendText: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await act(async () => button('補充').click())
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'] ?? '', '')
})

test('群組：送出途中草稿被改過就保留，沒改過才清', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedGroup('@gb first prompt')
  const sent = gate<{ id: string } | null>()
  useStore.setState({ sendGroupChat: () => sent.promise } as never)
  await mount(<GroupChatPanel projectId="gp" onOpenSidebar={() => {}} />)
  await settle()
  await keydown(textarea(), 'Enter')
  await remoteDraft('group:gp', '@gb ' + NEW)
  await act(async () => sent.resolve({ id: 'chat-1' }))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'], '@gb ' + NEW, '群組送出成功後新草稿還在')
  assert.deepEqual(emptyPuts(requests), [])
})

test('群組：草稿沒被改過，成功後照常清空並同步空字串', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedGroup('@gb first prompt')
  const sent = gate<{ id: string } | null>()
  useStore.setState({ sendGroupChat: () => sent.promise } as never)
  await mount(<GroupChatPanel projectId="gp" onOpenSidebar={() => {}} />)
  await settle()
  await keydown(textarea(), 'Enter')
  await act(async () => sent.resolve({ id: 'chat-1' }))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'] ?? '', '')
  assert.equal(emptyPuts(requests).length, 1, '清空要同步回 daemon')
})

async function sendMission(text: string) {
  seedGroup(text)
  useStore.setState({ missionsSupported: true } as never)
  const started = gate<string | null>()
  useStore.setState({ startMission: () => started.promise } as never)
  await mount(<GroupChatPanel projectId="gp" onOpenSidebar={() => {}} />)
  await settle()
  await act(async () => document.querySelector<HTMLButtonElement>('.agm-toggle')!.click())
  await act(async () => document.querySelector<HTMLButtonElement>('.send-btn')!.click())
  return { started }
}

test('交給 AGM：送出途中草稿被改過就保留文字，沒改過才清', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  const { started } = await sendMission('@gb first prompt')
  await remoteDraft('group:gp', 'new text from another device')
  await act(async () => started.resolve('mission-1'))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'], 'new text from another device', '新草稿還在')
})

test('交給 AGM：草稿沒被改過，成功後照常清空', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  const { started } = await sendMission('@gb first prompt')
  await act(async () => started.resolve('mission-2'))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'] ?? '', '')
})

// 草稿追蹤的訂閱與 React 自己的訂閱都走 `useStore.subscribe`：包一層記下還活著的，數目回到原值就代表都解除了。
const liveListeners = new Set<() => void>()
const realSubscribe = useStore.subscribe
before(() => {
  ;(useStore as unknown as { subscribe: typeof realSubscribe }).subscribe = (listener) => {
    const unsubscribe = realSubscribe(listener)
    const tracked = () => {
      liveListeners.delete(tracked)
      unsubscribe()
    }
    liveListeners.add(tracked)
    return tracked
  }
})
after(() => {
  ;(useStore as unknown as { subscribe: typeof realSubscribe }).subscribe = realSubscribe
})

test('一般送出：送出途中本機打字也保留，成功後訂閱解除', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle()
  const idle = liveListeners.size
  await keydown(textarea(), 'Enter')
  await act(async () => useStore.getState().setDraft('bot:b', 'typed locally'))
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], 'typed locally', '本機打字還在')
  assert.deepEqual(emptyPuts(requests), [])
  assert.equal(liveListeners.size, idle, '送出成功後追蹤訂閱已解除')
})

test('一般送出：送出失敗（resolve(false)）不清框，訂閱解除', { timeout: 30_000 }, async () => {
  const requests = fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle()
  const idle = liveListeners.size
  await keydown(textarea(), 'Enter')
  await act(async () => sent.resolve(false))
  await settle()
  assert.equal(useStore.getState().drafts['bot:b'], 'first prompt', '失敗後草稿照舊')
  assert.deepEqual(emptyPuts(requests), [])
  assert.equal(liveListeners.size, idle, '送出失敗後追蹤訂閱已解除')
})

test('一般送出：送出在途中卸載，回來後訂閱解除', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', false)
  const sent = gate<boolean>()
  useStore.setState({ sendPrompt: () => sent.promise } as never)
  const base = liveListeners.size
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle()
  await keydown(textarea(), 'Enter')
  await unmountAll()
  await act(async () => sent.resolve(true))
  await settle()
  assert.equal(liveListeners.size, base, '卸載後 ChatPanel 與送出追蹤的訂閱都回到原值')
})

test('群組：送出途中本機打字也保留；失敗（resolve(null)）不清框、訂閱解除', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedGroup('@gb first prompt')
  const sent = gate<{ id: string } | null>()
  useStore.setState({ sendGroupChat: () => sent.promise } as never)
  await mount(<GroupChatPanel projectId="gp" onOpenSidebar={() => {}} />)
  await settle()
  const idle = liveListeners.size
  await keydown(textarea(), 'Enter')
  await act(async () => useStore.getState().setDraft('group:gp', '@gb typed locally'))
  await act(async () => sent.resolve(null))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'], '@gb typed locally', '本機打字還在')
  assert.equal(liveListeners.size, idle, '送出失敗後追蹤訂閱已解除')
})

test('交給 AGM：建任務失敗（resolve(null)）不清框，訂閱解除', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedGroup('@gb first prompt')
  useStore.setState({ missionsSupported: true } as never)
  const started = gate<string | null>()
  useStore.setState({ startMission: () => started.promise } as never)
  await mount(<GroupChatPanel projectId="gp" onOpenSidebar={() => {}} />)
  await settle()
  const idle = liveListeners.size
  await act(async () => document.querySelector<HTMLButtonElement>('.agm-toggle')!.click())
  await act(async () => document.querySelector<HTMLButtonElement>('.send-btn')!.click())
  await act(async () => started.resolve(null))
  await settle()
  assert.equal(useStore.getState().drafts['group:gp'], '@gb first prompt', '失敗後草稿照舊')
  assert.equal(liveListeners.size, idle, '失敗後追蹤訂閱已解除')
})

test('一般送出：送出連線失敗（傳輸層 reject）不清框，訂閱解除', { timeout: 30_000 }, async () => {
  fakeApi(() => undefined)
  seedBot('first prompt', false)
  // 不覆寫 `sendPrompt`：讓 store 的正式送出流程處理傳輸層的 reject（它自己接住，回 false），跟線上一樣。
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    if (init?.method === 'POST' && /\/prompt/.test(String(input))) throw new TypeError('network down')
    return realFetch(input, init as RequestInit)
  }) as typeof fetch
  try {
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await settle()
    const idle = liveListeners.size
    await keydown(textarea(), 'Enter')
    await settle()
    assert.equal(useStore.getState().drafts['bot:b'], 'first prompt', '連線失敗後草稿照舊')
    assert.equal(liveListeners.size, idle, '連線失敗後追蹤訂閱已解除')
  } finally {
    globalThis.fetch = realFetch
  }
})
