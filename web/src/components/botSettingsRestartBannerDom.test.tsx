/**
 * Bot 設定面板的「重啟後生效」橫幅只看 daemon 的判斷（PATCH 回應的 `needs_restart`，live_apply 成功就是 false）。
 * claude 改 effort 由 daemon 在跑著的 session 裡當場套用（`/effort`），不能一儲存就跳「已儲存，重啟 Bot 後生效」：
 * daemon 先寫 config 推 `bot_changed`、再套用，這段 state 的 `bot.needs_restart` 暫時是真的（2026-10-05 使用者截圖）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, click, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const bot = {
  id: 'b1', name: 'wits-pro', project_id: 'p1', kind: 'claude', model: 'opus', effort: 'medium', fast: false,
  identity: null, persona: null, live_apply_deferred: false, needs_restart: false,
} as unknown as Bot
const run = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' } as unknown as Run

/** PATCH 卡住等測試放行；其他請求回空物件。`refreshState` 換成空的，state 由測試自己擺。 */
function holdPatch(): { release: (body: unknown) => void; sent: unknown[] } {
  const sent: unknown[] = []
  let release: (body: unknown) => void = () => {}
  globalThis.fetch = (async (_input: string, init?: { method?: string; body?: string }) => {
    if (init?.method === 'PATCH') {
      sent.push(JSON.parse(init.body ?? '{}'))
      const body = await new Promise<unknown>((resolve) => (release = resolve))
      return new Response(JSON.stringify(body), { status: 200 })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch
  useStore.setState({ refreshState: async () => {} })
  return { release: (b) => release(b), sent }
}

const text = () => document.body.textContent ?? ''
const buttonByText = (label: string) =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim().startsWith(label))!

async function saveEffortHigh(): Promise<void> {
  await click(buttonByText('High'))
  await click(buttonByText('儲存'))
}

test('claude 改 effort：儲存中（state 暫時 needs_restart）與套用成功後都不出「重啟後生效」橫幅', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  const api = holdPatch()
  await mount(<BotSettingsPanel botId="b1" />)
  await saveEffortHigh()
  await until(() => api.sent.length === 1, 'PATCH 送出')
  assert.deepEqual(api.sent[0], { effort: 'high' })
  // daemon 套用前推的 bot_changed 讓 state 暫時算出 needs_restart
  await act(async () => {
    useStore.setState({ bots: [{ ...bot, effort: 'high', needs_restart: true } as Bot] })
  })
  assert.ok(!text().includes('重啟 Bot 後生效'), '套用中不能先跳重啟橫幅')
  assert.ok(!document.querySelector('.bs-banner.warn'), '套用中沒有警告橫幅')
  assert.ok(text().includes('套用中'), '改顯示套用中')
  await act(async () => {
    api.release({ needs_restart: false, live_apply: { fields: ['effort'], applied: true, deferred: false, pending_bookkeeping: false, reason: null } })
    useStore.setState({ bots: [{ ...bot, effort: 'high', needs_restart: false } as Bot] })
  })
  await until(() => text().includes('已套用，不用重啟'), '套用完成的提示')
  assert.ok(!text().includes('重啟 Bot 後生效'))
  assert.ok(!text().includes('套用中'))
})

test('daemon 回 needs_restart:true（要重啟的欄位／套用失敗）：才出「重啟後生效」橫幅與立即重啟', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  const api = holdPatch()
  await mount(<BotSettingsPanel botId="b1" />)
  await saveEffortHigh()
  await until(() => api.sent.length === 1, 'PATCH 送出')
  await act(async () => {
    api.release({ needs_restart: true, live_apply: { fields: ['effort'], applied: false, deferred: false, pending_bookkeeping: false, reason: 'slash_send_failed' } })
    useStore.setState({ bots: [{ ...bot, effort: 'high', needs_restart: true } as Bot] })
  })
  await until(() => text().includes('重啟 Bot 後生效'), '重啟橫幅')
  assert.ok(buttonByText('立即重啟'), '有立即重啟鈕')
})
