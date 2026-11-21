import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { resetStoreForTest, useStore } from '../store/store'

before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
afterEach(unmountAll)

const { MemPopover } = await import('./MemPopover')

function stubMem() {
  const realFetch = globalThis.fetch
  const gates: { resolve: (body: unknown) => void }[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (!path.startsWith('/mem/processes')) return new Response('{}', { status: 404 })
    const body = await new Promise<unknown>((resolve, reject) => {
      gates.push({ resolve })
      init?.signal?.addEventListener('abort', () => reject(init.signal?.reason ?? new DOMException('Aborted', 'AbortError')), { once: true })
    })
    return new Response(JSON.stringify(body), { status: 200 })
  }) as unknown as typeof fetch
  return { gates, restore: () => void (globalThis.fetch = realFetch) }
}

const snap = (name: string) => ({
  host: 'local',
  sampled_at: new Date().toISOString(),
  processes: [
    { pid: 1, ppid: 0, rss_bytes: 1024, exe: 'bot', argv: 'bot', pane_id: null, socket_path: null, bot_id: 'b1', bot_name: name, project_id: null, owner: 'bot', subtree_bytes: 1024, children: 0 },
  ],
})

async function flush(rounds = 5) {
  for (let i = 0; i < rounds; i++) await act(async () => new Promise<void>((r) => setImmediate(r)))
}

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

async function openPopover() {
  const root = await mount(
    <MemPopover host="local">
      <span>RAM</span>
    </MemPopover>,
  )
  await click(root.querySelector('.mem-open')!)
  await flush()
  return root
}

test('上一趟還沒回來不發第二趟：到點也不疊，前一趟回來後才照排下一趟', async () => {
  const f = stubMem()
  const timers = installManualTimers()
  try {
    await openPopover()
    assert.equal(f.gates.length, 1, '點開就抓一趟')
    await act(async () => timers.clock.advance(10_000))
    assert.equal(f.gates.length, 1, '第一趟尚未逾時：不發重疊請求')
    f.gates[0]!.resolve(snap('bot-one'))
    await flush()
    await act(async () => timers.clock.advance(15_000))
    assert.equal(f.gates.length, 2, '第一趟回來後，下一趟照排')
    await act(async () => timers.clock.advance(10_000))
    assert.equal(f.gates.length, 2, '第二趟尚未逾時：不發重疊請求')
  } finally {
    timers.restore()
    f.restore()
  }
})

test('分頁在背景不抓，回前景立刻補一次', async () => {
  const f = stubMem()
  const timers = installManualTimers()
  try {
    await openPopover()
    f.gates[0]!.resolve(snap('bot-one'))
    await flush()
    setVisibility('hidden')
    await act(async () => timers.clock.advance(60_000))
    await flush()
    assert.equal(f.gates.length, 1, '背景：到點也不抓（daemon 不必替看不見的分頁跑 ps，遠端還要 ssh）')
    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await flush()
    assert.equal(f.gates.length, 2, '回前景立刻補一次，不必等下一個 15 秒')
  } finally {
    timers.restore()
    f.restore()
    delete (document as unknown as { visibilityState?: string }).visibilityState
  }
})

test('晚回來的舊清單不蓋掉新的：兩趟都在路上，先回來的是新的，舊的晚到也不能蓋回去', async () => {
  const f = stubMem()
  const timers = installManualTimers()
  useStore.setState({ stopBot: (async () => {}) as never })
  try {
    await openPopover()
    f.gates[0]!.resolve(snap('bot-A'))
    await flush()
    await click(document.querySelector<HTMLButtonElement>('.mem-pop-refresh')!)
    await flush()
    assert.equal(f.gates.length, 2)
    const stop = () => document.querySelector<HTMLButtonElement>('.mem-pop-act button')!
    await click(stop())
    await click(stop())
    await flush()
    assert.equal(f.gates.length, 3, '停完重抓一趟')
    f.gates[2]!.resolve(snap('bot-B'))
    await flush()
    assert.match(document.body.textContent ?? '', /bot-B/)
    f.gates[1]!.resolve(snap('bot-A'))
    await flush()
    assert.match(document.body.textContent ?? '', /bot-B/, '舊的那趟晚到，畫面仍是最新的')
    assert.doesNotMatch(document.body.textContent ?? '', /bot-A/, '舊清單（已被殺掉的那顆）不能又出現')
  } finally {
    timers.restore()
    f.restore()
  }
})

test('單趟請求逾時後回前景可補抓，之後輪詢也會繼續', async () => {
  const f = stubMem()
  const timers = installManualTimers()
  try {
    await openPopover()
    assert.equal(f.gates.length, 1, '點開就發出第一趟')
    await act(async () => timers.clock.advance(15_000))
    await flush()

    setVisibility('visible')
    await act(async () => document.dispatchEvent(new Event('visibilitychange')))
    await flush()
    assert.equal(f.gates.length, 2, '首趟逾時後 inflight 歸零，回前景立即補抓')

    f.gates[1]!.resolve(snap('bot-after-timeout'))
    await flush()
    await act(async () => timers.clock.advance(15_000))
    await flush()
    assert.equal(f.gates.length, 3, '補抓完成後週期輪詢繼續')
  } finally {
    timers.restore()
    f.restore()
    delete (document as unknown as { visibilityState?: string }).visibilityState
  }
})
