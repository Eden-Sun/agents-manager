/**
 * 記憶體彈窗的「停止 bot」失敗：daemon 的理由要當 alert 念出來（按鈕還在、焦點在它身上，使用者得知道沒停成）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { MemProcess } from '../api/types'
import { RowAction } from './MemPopover'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
after(teardownDom)

const proc = {
  pid: 123, ppid: 1, rss_bytes: 1024, exe: 'claude', argv: 'claude', pane_id: null, socket_path: null,
  bot_id: 'b1', bot_name: 'alpha', project_id: 'p1', owner: 'bot', tree_rss_bytes: 1024,
} as unknown as MemProcess

test('停止 bot 失敗：daemon 的理由是 role=alert，讀屏會念出來', async () => {
  useStore.setState({ stopBot: async () => { throw new Error('bot 正在做事，停止被拒絕') } } as never)
  await mount(<RowAction p={proc} host="local" onDone={() => {}} />)
  const stop = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.includes('停止 bot'))!
  await click(stop)
  await click(stop)
  await settle(20)
  const err = document.querySelector<HTMLElement>('.mem-pop-err')
  assert.equal(err?.textContent, 'bot 正在做事，停止被拒絕', '失敗的理由要顯示')
  assert.equal(err.getAttribute('role'), 'alert')
})
