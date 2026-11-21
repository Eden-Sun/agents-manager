import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, click, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { BotSettingsPanel } from './BotSettingsPanel'
import { BotRowMenu } from './BotRowMenu'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const bot = {
  id: 'b1', name: 'wits-pro', project_id: 'p1', kind: 'codex', model: 'gpt-6-sol', effort: 'high', fast: false,
  identity: null, persona: null, live_apply_deferred: false, needs_restart: false,
} as unknown as Bot
const run = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle' } as unknown as Run

const buttons = () => [...document.querySelectorAll<HTMLButtonElement>('.bs-actions button')]
const footerButton = (label: string) => buttons().find((b) => b.textContent?.trim() === label)

test('沒有橫幅（沒改設定）也有常駐「重新啟動」，點了呼叫 restartBot 接回原對話', async () => {
  const restarted: string[] = []
  useStore.setState({
    bots: [bot],
    runs: { b1: run },
    busy: {},
    restartBot: async (id: string) => { restarted.push(id); return true },
  })
  await mount(<BotSettingsPanel botId="b1" />)
  assert.ok(!document.querySelector('.bs-banner.warn'), '沒有任何警告橫幅')
  const btn = footerButton('重新啟動')
  assert.ok(btn, '底部要有重新啟動')
  await click(btn)
  await until(() => restarted.length === 1, '呼叫 restartBot')
  assert.deepEqual(restarted, ['b1'])
})

test('沒有 live run：不出現「重新啟動」，改成「啟動」', async () => {
  useStore.setState({ bots: [bot], runs: {}, busy: {} })
  await mount(<BotSettingsPanel botId="b1" />)
  assert.equal(footerButton('重新啟動'), undefined)
  assert.ok(footerButton('啟動'), '沒在跑時改成啟動')
})

test('child bot 不出現重新啟動（父 Bot 重開，/restart 回 409）', async () => {
  const child = { ...bot, id: 'c1', managed_by: 'child', parent_bot_id: 'b1' } as unknown as Bot
  useStore.setState({ bots: [child], runs: { c1: { ...run, bot_id: 'c1' } as Run }, busy: {} })
  await mount(<BotSettingsPanel botId="c1" />)
  assert.equal(footerButton('重新啟動'), undefined)
})

test('claude-api bot 不出現重新啟動', async () => {
  const api = { ...bot, id: 'api1', kind: 'claude-api', model: 'claude-opus-5-5' } as unknown as Bot
  useStore.setState({ bots: [api], runs: { api1: { ...run, bot_id: 'api1' } as Run }, busy: {} })
  await mount(<BotSettingsPanel botId="api1" />)
  assert.equal(footerButton('重新啟動'), undefined)
})

test('側欄 bot 的 ⋯ 選單：執行中有「重新啟動」，沒執行中沒有', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {}, projects: [] })
  await mount(<BotRowMenu botId="b1" />)
  await click(document.querySelector<HTMLButtonElement>('.bot-actions button')!)
  await until(() => [...document.querySelectorAll('[role="menuitem"]')].some((el) => el.textContent?.trim() === '重新啟動'), '選單出現重新啟動')
  await unmountAll()

  useStore.setState({ bots: [bot], runs: {}, busy: {}, projects: [] })
  await mount(<BotRowMenu botId="b1" />)
  await click(document.querySelector<HTMLButtonElement>('.bot-actions button')!)
  await until(() => [...document.querySelectorAll('[role="menuitem"]')].some((el) => el.textContent?.trim() === '啟動'), '選單出現啟動')
  assert.equal([...document.querySelectorAll('[role="menuitem"]')].find((el) => el.textContent?.trim() === '重新啟動'), undefined)
})
