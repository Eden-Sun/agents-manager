/**
 * 輸入框上方的「背景執行中」：手機只畫一行摘要（2026-10-03 使用者：藍色那塊太佔空間），說明句收在可點開的區塊。
 * 版面本身靠 CSS（≤640px 藏 `.bg-jobs-detail`），這裡釘結構：摘要是按鈕、說明在摘要外、點了切 `open`。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { BackgroundJobsBar } from './BackgroundJobs'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(() => {
  resetStoreForTest()
  teardownDom()
})

const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as unknown as Bot
const since = new Date(Date.now() - 5 * 60_000).toISOString()
const run = {
  id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle', background_jobs: 1, background_since: since,
  background_tasks: [{ id: 't1', type: 'shell', description: 'Watch luna children' }],
} as unknown as Run

test('摘要一行：狀態、跑多久、工作名在按鈕裡，說明句在外面；點一下展開', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  await mount(<BackgroundJobsBar botId="b1" />)
  const bar = document.querySelector<HTMLElement>('.bg-jobs-bar')!
  const summary = bar.querySelector<HTMLButtonElement>('button.bg-jobs-summary')!
  assert.ok(summary.textContent?.includes('背景執行中（1）'))
  assert.ok(summary.textContent?.includes('5 分鐘'))
  assert.ok(summary.textContent?.includes('shell：Watch luna children'))
  const detail = bar.querySelector('.bg-jobs-detail')!
  assert.ok(!summary.contains(detail), '說明不在摘要列裡，手機才藏得掉')
  assert.ok(detail.textContent?.includes('回合已經結束'))
  assert.equal(summary.getAttribute('aria-expanded'), 'false')
  await act(async () => summary.click())
  assert.ok(bar.classList.contains('open'))
  assert.equal(summary.getAttribute('aria-expanded'), 'true')
})
