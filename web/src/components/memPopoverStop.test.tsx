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

test('bot 的子行程（例如 bash, node）不顯示停止按鈕', async () => {
  const child = { owner: 'bot', bot_id: 'b1', bot_name: 'b1', pid: 43, exe: 'bash' } as MemProcess
  await mount(<RowAction p={child} host="local" onDone={() => {}} />)
  assert.equal(document.querySelector('.mem-pop-act button'), null, '子行程不該顯示停止按鈕')
})
