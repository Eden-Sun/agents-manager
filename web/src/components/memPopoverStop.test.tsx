/**
 * 記憶體清單裡的「停止 bot」：一下就停掉一顆正在跑的 bot（會中斷它的回合），不能一個誤觸就執行。
 * 跟旁邊 SIGTERM → 強制 的兩段式同一個做法：第一下只是「武裝」，要再按一次；放著不動會自己解除。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { MemProcess } from '../api/types'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const { RowAction } = await import('./MemPopover')
const p = { owner: 'bot', bot_id: 'b1', bot_name: 'b1', pid: 42 } as MemProcess

test('停止 bot：第一下只武裝（按鈕變「再按一次確定停止」），第二下才真的停；只停一次', async () => {
  const stopped: string[] = []
  useStore.setState({ stopBot: (async (id: string) => void stopped.push(id)) as never })
  await mount(<RowAction p={p} host="local" onDone={() => {}} />)
  const btn = () => document.querySelector<HTMLButtonElement>('.mem-pop-act button')!
  assert.equal(btn().textContent, '停止 bot')
  await click(btn())
  assert.deepEqual(stopped, [], '第一下不停')
  assert.match(btn().textContent ?? '', /確定停止/)
  await click(btn())
  await click(btn())
  assert.deepEqual(stopped, ['b1'], '第二下停，連點不重複')
})
