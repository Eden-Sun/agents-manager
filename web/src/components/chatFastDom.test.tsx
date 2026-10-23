/**
 * #1914 追加：聊天標題列的「· fast」與手機 logo 的閃電，跟側欄與選單一樣看 `runningFast`（runtime → 設定）。
 * run 走真的 `toRun` 正規化（daemon 形狀的 `status_json` 字串），不手塞 status。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { toRun } from '../api/normalize'
import type { Bot, Run } from '../api/types'
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

/** 手機寬度：`matchMedia` 對 640px 斷點回 true（同 `mobileGitInfoDom`）。 */
function stubPhone(phone: boolean): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: phone && q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

/** 開聊天並把 bot 的設定與 run 換成這個測試要的樣子（run 經 `toRun` 正規化）。 */
async function openWith(botName: string, opts: { phone: boolean; botFast: boolean; statusJson: unknown; runtimeFast: 0 | 1 }) {
  const restore = stubPhone(opts.phone)
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === botName) as Bot
  assert.ok(bot, `mock 要有 ${botName}`)
  const run = toRun({
    id: `run-${bot.id}`,
    bot_id: bot.id,
    state: 'running',
    agent_status: 'idle',
    status_json: JSON.stringify(opts.statusJson),
    runtime_fast: opts.runtimeFast,
  }) as Run
  await act(async () => {
    useStore.setState((s) => ({
      // 標題列的 model 標籤要有模型才會渲染（`bot.model || model_name`），這裡給一個設定值。
      bots: s.bots.map((b) => (b.id === bot.id ? { ...b, fast: opts.botFast, model: b.kind === 'codex' ? 'gpt-6-luna' : b.model } : b)),
      runs: { ...s.runs, [bot.id]: run },
    }))
  })
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle(200)
  restore()
  return bot
}

const modelExtra = () => document.querySelector<HTMLElement>('.model-tag-extra')?.textContent ?? ''
const mobileBoltCodex = () => document.querySelector<HTMLElement>('.mobile-kind-icon.codex.fast')

test('codex：status_json 只有 context_window、runtime_fast=1（設定是關）→ 標題列有 fast，手機 logo 有閃電', async () => {
  await openWith('am-codex', { phone: false, botFast: false, statusJson: { context_window: { used_percentage: 12 } }, runtimeFast: 1 })
  assert.equal(modelExtra(), 'fast', '標題列的 model 標籤要寫 fast')
  await unmountAll()
  await openWith('am-codex', { phone: true, botFast: false, statusJson: { context_window: { used_percentage: 12 } }, runtimeFast: 1 })
  assert.ok(mobileBoltCodex(), '手機 logo 要有閃電')
})

test('codex：同樣條件但 runtime_fast=0（設定是開）→ 標題列與手機 logo 都沒有 fast', async () => {
  await openWith('am-codex', { phone: false, botFast: true, statusJson: { context_window: { used_percentage: 12 } }, runtimeFast: 0 })
  assert.equal(modelExtra(), '', '實際沒在跑 fast：以 runtime 為準，不看設定')
  await unmountAll()
  await openWith('am-codex', { phone: true, botFast: true, statusJson: { context_window: { used_percentage: 12 } }, runtimeFast: 0 })
  assert.equal(mobileBoltCodex(), null, '手機 logo 沒有閃電')
})

test('claude：status_json 帶 fast_mode:false → 標題列不顯示 fast（CLI 回報優先）', async () => {
  await openWith('am-claude', { phone: false, botFast: false, statusJson: { fast_mode: false, context_window: {} }, runtimeFast: 1 })
  assert.equal(modelExtra().includes('fast'), false, 'CLI 說沒開 fast，就不寫 fast')
})
