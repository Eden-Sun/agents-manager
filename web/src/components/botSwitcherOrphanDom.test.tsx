/**
 * 換 bot 選單與側欄一樣：子 agent 不單獨列（它掛在母 bot 底下），但父列不在清單裡的孤兒子 agent 沒有地方掛，
 * 要照頂層 bot 列出來；不然手機上這顆 bot 只剩鍵盤找得到（側欄的孤兒處理見 #1538）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Project } from '../api/types'
import { BotSwitcher } from './BotSwitcher'

afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

test('父列不在清單裡的孤兒子 agent 也列在換 bot 選單裡；有父列的子 agent 不列', async () => {
  resetStoreForTest()
  useStore.setState({
    projects: [{ id: 'P1', label: 'P1', path: '/p1', host: 'local' } as Project],
    bots: [
      { id: 'b1', name: 'b1', project_id: 'P1', kind: 'claude', identity: null, parent_bot_id: null } as Bot,
      { id: 'k1', name: 'k1', project_id: 'P1', kind: 'claude', identity: null, parent_bot_id: 'b1' } as Bot,
      { id: 'o1', name: 'o1', project_id: 'P1', kind: 'codex', identity: null, parent_bot_id: 'GONE' } as Bot,
    ],
    botOrder: {},
    projectOrder: [],
  })
  await mount(<BotSwitcher botId="b1" name="b1" />)
  await click(document.querySelector<HTMLButtonElement>('.bot-switcher-btn')!)
  await settle(20)
  const names = [...document.querySelectorAll('.bot-switcher-name')].map((el) => el.textContent)
  assert.ok(names.includes('o1'), `孤兒要列出來，實際 ${JSON.stringify(names)}`)
  assert.ok(!names.includes('k1'), '有父列的子 agent 不單獨列')
})
