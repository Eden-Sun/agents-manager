/**
 * 側欄 Bot 列可以用 Alt＋↑／↓ 調整順序（鍵盤替代拖曳）：這個操作要有說明，讀屏使用者才找得到。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { Sidebar } from './Sidebar'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

const bot = (id: string) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})

test('Bot 列掛著 Alt＋↑↓ 的說明（aria-describedby 指到一段讀屏看得到的文字）', async () => {
  fakeApi(() => undefined)
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj-p1', path: '/p1', host: 'local' }],
    bots: [bot('b1'), bot('b2')], identities: [], runs: {}, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'b1', selectedProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
  await mount(<Sidebar />)
  await settle()
  const row = document.querySelector<HTMLElement>('.bot-row[data-bot-id="b1"]')!
  assert.ok(row, '要畫出 Bot 列')
  const hint = row.getAttribute('aria-describedby') ? document.getElementById(row.getAttribute('aria-describedby')!) : null
  assert.ok(hint?.textContent?.includes('Alt'), 'Bot 列的 aria-describedby 要指到含 Alt 的說明')
})
