/** 主力 bot 狀態卡（2026-10-04 使用者：「hover 至主力的 bot 時，就說明目前 context 狀態」）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { BotStatusCard, type ChipHints } from './BotStatusCard'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(() => {
  resetStoreForTest()
  teardownDom()
})

const bot = { id: 'b1', name: 'cicd', project_id: 'p1', kind: 'claude', model: 'claude-opus-5-5', effort: 'high', identity: 'cc0' } as unknown as Bot
const run = {
  id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'working', agent_title: '跑整樹測試',
  status: { context_used_pct: 42.4, context_used_tokens: 84000, context_size: 200000, model_name: 'Opus 5.5', effort: 'high' },
} as unknown as Run
const hints: ChipHints = { current: false, unread: 0, needsReply: false, waitsKids: false, kidsRunning: 2, cacheTitle: null }

test('卡片寫出在做什麼、context 用量、子 agent 與模型', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  await mount(<BotStatusCard botId="b1" hints={hints} anchor={null} onClose={() => {}} onLegend={() => {}} />)
  const card = document.querySelector('.bot-status-card')!
  const text = card.textContent ?? ''
  assert.ok(text.includes('跑整樹測試'), text)
  assert.ok(text.includes('已用 42%（84k / 200k）'), text)
  assert.ok(text.includes('2 個在跑'), text)
  assert.ok(text.includes('cc0'), text)
})

test('標題句跟晶片顏色同一個優先序：要你回答最先', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  await mount(<BotStatusCard botId="b1" hints={{ ...hints, needsReply: true, unread: 3 }} anchor={null} onClose={() => {}} onLegend={() => {}} />)
  assert.ok(document.querySelector('.bot-status-headline')!.textContent!.startsWith('停在等你回答'))
})
