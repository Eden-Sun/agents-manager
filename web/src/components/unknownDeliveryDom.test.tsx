/**
 * issue #1479：`delivery=unknown` 的提示要看 bot 有沒有在跑。
 * - 在跑（`agent_status=working`）：中性的「送達尚未確認」，沒有「放棄該回合」這顆按鈕；
 * - 閒著、輸入框仍鎖著：維持原本的「請先放棄該回合」＋按鈕。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import type { Run, Turn } from '../api/types'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  await advanceMockTime(10_000)
})
before(async () => {
  await setupDom()
  await advanceMockTime(10_000)
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

async function openChat(name: string) {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === name)!
  assert.ok(bot, `mock 要有 ${name}`)
  if (useStore.getState().runs[bot.id]?.state !== 'running') {
    await sharedMock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running'
    }, `${name} running`)
  }
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle(200)
  return bot
}

async function lockWith(botId: string, agentStatus: Run['agent_status']) {
  await act(async () => {
    useStore.setState((s) => {
      const run = s.runs[botId]!
      const turn = { id: 'turn-unknown', run_id: run.id, status: 'in_flight', delivery: 'unknown', created_at: new Date().toISOString() } as unknown as Turn
      return {
        runs: { ...s.runs, [botId]: { ...run, agent_status: agentStatus } },
        turns: { ...s.turns, [botId]: { ...(s.turns[botId] ?? {}), 'turn-unknown': turn } },
      }
    })
  })
  await settle(50)
}

const lock = () => document.querySelector<HTMLElement>('.composer-lock')
const buttons = () => [...(lock()?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim())

test('unknown + working：中性提示，沒有「放棄該回合」', { timeout: 30_000 }, async () => {
  const bot = await openChat('am-claude')
  await lockWith(bot.id, 'working')
  assert.ok(lock(), '輸入框上方有鎖定提示')
  assert.match(lock()!.textContent ?? '', /Bot 正在處理，送達尚未確認/)
  assert.doesNotMatch(lock()!.textContent ?? '', /請先「放棄該回合」/)
  assert.ok(!buttons().includes('放棄該回合'), `按鈕：${buttons().join('、')}`)
})

test('unknown + idle：維持原本的放棄提示與按鈕', { timeout: 30_000 }, async () => {
  const bot = await openChat('am-claude')
  await lockWith(bot.id, 'idle')
  assert.match(lock()!.textContent ?? '', /上一回合的送達狀態未知，請先「放棄該回合」或停止 Bot/)
  assert.ok(buttons().includes('放棄該回合'), `按鈕：${buttons().join('、')}`)
})
