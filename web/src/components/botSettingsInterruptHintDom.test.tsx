/**
 * codex 回合進行中改 model／effort，儲存會先按 Esc 停下回合、套用後送 continue（#1681）：儲存前就要說明。
 * 只改 fast、回合沒在跑、不是 codex 都不出現這條說明。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, click, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
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

const botOf = (kind: 'codex' | 'claude') =>
  ({
    id: 'b1', name: 'sol', project_id: 'p1', kind, model: 'gpt-6-luna', effort: 'medium', fast: false,
    identity: null, persona: null, live_apply_deferred: false, needs_restart: false,
  }) as unknown as Bot
const runOf = (agent_status: string) => ({ id: 'r1', bot_id: 'b1', state: 'running', agent_status }) as unknown as Run

const text = () => document.body.textContent ?? ''
const buttonByText = (label: string) =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim().startsWith(label))!

async function open(kind: 'codex' | 'claude', agent: string): Promise<void> {
  globalThis.fetch = (async () => new Response('{}', { status: 200 })) as unknown as typeof fetch
  useStore.setState({ bots: [botOf(kind)], runs: { b1: runOf(agent) }, busy: {}, refreshState: async () => {} })
  await mount(<BotSettingsPanel botId="b1" />)
}

test('codex 回合進行中改 effort：儲存前就說明會先停下目前回合、套用後送 continue', async () => {
  await open('codex', 'working')
  assert.ok(!text().includes('先按 Esc'), '還沒改任何值：不出現')
  await click(buttonByText('High'))
  assert.ok(text().includes('先按 Esc 停下目前回合') && text().includes('continue'), '改了 effort 才出現')
})

test('codex 閒著、或不是 codex：改 effort 不出現打斷說明', async () => {
  await open('codex', 'idle')
  await click(buttonByText('High'))
  assert.ok(!text().includes('先按 Esc'), 'codex 閒著')
  await act(async () => {
    await unmountAll()
  })
  await open('claude', 'working')
  await click(buttonByText('High'))
  assert.ok(!text().includes('先按 Esc'), 'claude 不打斷')
})
