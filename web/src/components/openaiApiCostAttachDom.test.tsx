import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { flush } from '../testing/flush'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { ApiTurnCostTag, ApiTurnProgress } from './ApiTurnCost'
import { ComposerCostHint } from './ComposerCostHint'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const oaBot = { id: 'oa1', name: 'oa', kind: 'openai-api', project_id: 'p1', model: 'gpt-6-luna', effort: 'medium', fast: false, primary: false, managed_by: 'user', parent_bot_id: null, role: null, track: null, identity: null, args: [], env: {}, autostart: false, inject_hooks: true, auto_approve: false, cwd: null, runtime_override: null, primary_position: 0, manager_use: true } as never

const usage = (turns: unknown[]) => ({
  attempts: { total: 1, recorded: 1, reserved: 1, unknown: 0, failed: 0 },
  cost_microusd: { recorded: 12_300, reserved: 90_000, unknown: 0 },
  tokens: { input: 1000, output: 300, cache_read: 200, cache_creation: 50 },
  cache_hit_rate: null,
  last: null,
  turns,
})

function stubUsage(turns: unknown[]) {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) return usage(turns)
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  return () => {
    rawTransport.request = orig
  }
}

const done = {
  turn_id: 't1', requests: 2, cost_usd: 0.0123, reserved_usd: 0, in_flight: false,
  input_tokens: 1000, output_tokens: 300, cache_read_tokens: 200, cache_write_tokens: 50, reasoning_tokens: 120,
  speed: null, started_at: null, max_calls: null, turn_timeout_secs: null,
}

test('回合後：openai 顯示實際費用與拆分，reasoning 另列；不是 Claude 的「第 N 次請求」字樣', async () => {
  useStore.setState({ bots: [oaBot] } as never)
  const restore = stubUsage([done])
  try {
    await mount(<ApiTurnCostTag botId="oa1" turnId="t1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
    const breakdown = document.querySelector('[data-k="openai-breakdown"]')?.textContent ?? ''
    assert.match(breakdown, /輸入 1000 · 快取讀 200 · 快取寫 50 · 輸出 300/)
    assert.match(breakdown, /reasoning 120（已含在輸出）/)
    assert.match(document.body.textContent ?? '', /\$0\.012/)
  } finally {
    restore()
  }
})

test('進行中：openai 不顯示即時估價 ≈，只顯示已結算＋預留', async () => {
  useStore.setState({
    bots: [oaBot],
    turns: { oa1: { t1: { id: 't1', status: 'in_flight' } as never } },
    liveReply: { oa1: { turnId: 't1', text: '', activity: '', alert: '', revision: 3, estCostMicrousd: 12_345 } },
  } as never)
  const restore = stubUsage([{ ...done, in_flight: true, reserved_usd: 0.09 }])
  try {
    await mount(<ApiTurnProgress botId="oa1" />)
    await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
    const text = document.querySelector('.msg-cost')?.textContent ?? ''
    assert.doesNotMatch(text, /≈/)
    assert.match(text, /已花 \$0\.012/)
    assert.match(document.querySelector('[data-k="turn-reserved"]')?.textContent ?? '', /預留 \$0\.090/)
  } finally {
    restore()
  }
})

test('輸入框上方：openai 沒有 Claude 快取冷提示', async () => {
  useStore.setState({ bots: [oaBot], selectedBotId: 'oa1' } as never)
  await mount(<ComposerCostHint botId="oa1" />)
  await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
  assert.equal(document.querySelector('.composer-cost.cold'), null)
})

test('附件：openai 的拖放與貼上都不進托盤，並顯示「不支援附件」', async () => {
  mockApi(sharedMock)
  useStore.setState({ bots: [oaBot], selectedBotId: 'oa1' } as never)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await flush()
  const note = () => document.querySelector('[data-k="openai-no-attach"]')
  assert.equal(note()?.textContent, 'OpenAI API bot 不支援附件', '應出現不支援附件提示')

  const file = new File([new Uint8Array([1, 2, 3])], 'photo.png', { type: 'image/png' })
  const chat = document.querySelector('.chat')!
  const drop = new Event('drop', { bubbles: true, cancelable: true })
  Object.defineProperty(drop, 'dataTransfer', { value: { files: [file], types: ['Files'], getData: () => '' } })
  await act(async () => {
    chat.dispatchEvent(drop)
  })
  await flush()
  assert.equal(document.querySelector('.attach-tray'), null, '拖放不產生附件')

  const paste = new Event('paste', { bubbles: true, cancelable: true })
  Object.defineProperty(paste, 'clipboardData', { value: { files: [file], types: ['image/png'], getData: () => '' } })
  await act(async () => {
    document.querySelector<HTMLTextAreaElement>('.composer textarea')!.dispatchEvent(paste)
  })
  await flush()
  assert.equal(document.querySelector('.attach-tray'), null, '貼上圖片不產生附件')
})
