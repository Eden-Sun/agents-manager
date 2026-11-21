import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { dialogOpen } from '../lib/dialogOpen'
import { ChipLegend } from './ChipLegend'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

beforeEach(() => {
  fakeApi()
  useStore.setState({
    bots: [{
      id: 'b1', name: 'bot-b1', project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
      autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: true, primary_position: 0, cwd: null,
    }],
    runs: {}, botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
  } as never)
})

function popBack(): void {
  const top = document.querySelector<HTMLElement>('[aria-modal]')!
  top.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
}

test('色票開著時 dialogOpen() 為真，上一頁派的 Escape 會關掉它', async () => {
  let closed = 0
  await mount(<ChipLegend onClose={() => closed++} />)
  assert.equal(dialogOpen(), true)
  await act(async () => popBack())
  assert.equal(closed, 1)
})
