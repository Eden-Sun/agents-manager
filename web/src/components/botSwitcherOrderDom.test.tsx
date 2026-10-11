/**
 * 換 bot 選單照側欄的順序（真的掛進 happy-dom）：專案與 bot 都套使用者拖出來的順序（`projectOrder`／`botOrder`），
 * 側欄收起來的（額度停用，`hiddenBotIds`）不列；正在看的那顆例外（#1243）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { quotaDisableKey, setQuotaDisabled } from '../store/quotaHide'
import type { Bot, Project } from '../api/types'
import { BotSwitcher } from './BotSwitcher'

/** b3 的帳號（identity）停用：收起來的走真的額度停用投影，不直接寫 `hiddenBotIds`（那會被 quotaHide 的 subscribe 蓋掉）。 */
const HIDE_KEY = quotaDisableKey('m4p', 'claude', 'acct3')

afterEach(async () => {
  setQuotaDisabled(HIDE_KEY, false, null)
  await unmountAll()
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

function seed() {
  resetStoreForTest()
  setQuotaDisabled(HIDE_KEY, true, null)
  useStore.setState({
    // daemon 回來的原始順序是 P1 在前；使用者把 P2 拖到最上面。
    projects: [
      { id: 'P1', label: 'P1', path: '/p1', host: 'm4p' } as Project,
      { id: 'P2', label: 'P2', path: '/p2', host: 'm4p' } as Project,
    ],
    bots: [
      { id: 'b1', name: 'b1', project_id: 'P1', kind: 'claude', identity: null } as Bot,
      { id: 'b2', name: 'b2', project_id: 'P1', kind: 'codex', identity: null } as Bot,
      { id: 'b3', name: 'b3', project_id: 'P1', kind: 'claude', identity: 'acct3' } as Bot,
      { id: 'c1', name: 'c1', project_id: 'P2', kind: 'claude', identity: null } as Bot,
    ],
    projectOrder: ['P2', 'P1'],
    botOrder: { P1: ['b2', 'b1', 'b3'] },
  })
}

async function openMenu() {
  await mount(<BotSwitcher botId="b1" name="b1" />)
  await click(document.querySelector<HTMLButtonElement>('.bot-switcher-btn')!)
  await settle(20)
}

test('選單照側欄的專案順序', async () => {
  seed()
  await openMenu()
  const projects = [...document.querySelectorAll('.bot-switcher-project')].map((el) => el.textContent ?? '')
  assert.equal(projects.length, 2)
  assert.ok(projects[0].includes('P2'), `第一個專案要是 P2，實際：${projects.join(' | ')}`)
  assert.ok(projects[1].includes('P1'), `第二個專案要是 P1，實際：${projects.join(' | ')}`)
})

test('專案內照拖曳後的 bot 順序、收起來的不列', async () => {
  seed()
  await openMenu()
  const groups = [...document.querySelectorAll<HTMLElement>('.bot-switcher-group')]
  const p1 = groups.find((g) => g.getAttribute('aria-label') === 'P1')!
  const names = [...p1.querySelectorAll('.bot-switcher-name')].map((el) => el.textContent)
  assert.deepEqual(names, ['b2', 'b1'], '照 botOrder；額度停用的 b3 不列')
  assert.equal(document.querySelector('.bot-switcher-pop')?.textContent?.includes('b3'), false, '選單裡沒有 b3')
})
