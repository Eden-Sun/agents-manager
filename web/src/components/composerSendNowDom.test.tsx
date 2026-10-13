import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { toRun } from '../api/normalize'
import { ChatPanel } from './ChatPanel'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

for (const [name, kind, supported, label] of [
  ['支援版本 claude', 'claude', true, '插隊'],
  ['舊版 claude', 'claude', false, null],
  ['版本未知 claude', 'claude', undefined, null],
  ['codex 旗標關', 'codex', false, null],
  ['codex 旗標開且版本足夠', 'codex', true, '插入'],
  ['codex 版本不足', 'codex', false, null],
  ['grok', 'grok', false, null],
  ['agy', 'agy', false, null],
] as const) {
  test(`#1672 ${name}：按 daemon 支援判斷顯示 send-now`, async () => {
    fakeApi(() => undefined)
    useStore.setState({
      connected: true, defaultConnected: true, selectedBotId: 'b1672',
      bots: [{ id: 'b1672', name, kind, project_id: 'p1672', herdr_session: 'default' }],
      projects: [{ id: 'p1672', path: '/tmp/send-now', host: 'local' }],
      runs: { b1672: toRun({ id: 'r1672', bot_id: 'b1672', state: 'running', agent_status: 'working', send_now_supported: supported }) },
      turns: { b1672: { t1672: { id: 't1672', run_id: 'r1672', status: 'in_flight', delivery: 'ok' } } },
      drafts: { 'bot:b1672': '補一句' }, messages: { b1672: [] },
    } as never)
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    const buttons = [...document.querySelectorAll<HTMLButtonElement>('.composer-lock button')]
    assert.ok(buttons.some((button) => button.textContent === 'ESC 中斷'))
    const sendNow = buttons.filter((button) => button.textContent === '插隊' || button.textContent === '插入')
    assert.deepEqual(sendNow.map((button) => button.textContent), label ? [label] : [])
    if (supported) {
      await act(() => useStore.setState((s) => ({ runs: { ...s.runs, b1672: { ...s.runs.b1672!, send_now_supported: false } } })))
      assert.equal([...document.querySelectorAll('.composer-lock button')].some((button) => button.textContent === label), false)
    }
  })
}
