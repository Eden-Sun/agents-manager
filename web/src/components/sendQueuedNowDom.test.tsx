import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { toRun } from '../api/normalize'
import { ChatPanel } from './ChatPanel'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

const LABEL = '送出排隊的補充'
for (const [name, draft, sentVia, supported, shown] of [
  ['有補充、輸入框空、支援', '', 'supplement', true, true],
  ['輸入框有字', '補一句', 'supplement', true, false],
  ['沒有補充', '', null, true, false],
  ['不支援 send-now', '', 'supplement', false, false],
] as const) {
  test(`送出排隊的補充鈕：${name}`, async () => {
    fakeApi(() => undefined)
    useStore.setState({
      connected: true, defaultConnected: true, selectedBotId: 'bq',
      bots: [{ id: 'bq', name, kind: 'claude', project_id: 'pq', herdr_session: 'default' }],
      projects: [{ id: 'pq', path: '/tmp/q', host: 'local' }],
      runs: { bq: toRun({ id: 'rq', bot_id: 'bq', state: 'running', agent_status: 'working', send_now_supported: supported }) },
      turns: { bq: { tq: { id: 'tq', run_id: 'rq', status: 'in_flight', delivery: 'ok' } } },
      drafts: { 'bot:bq': draft },
      messages: { bq: [{ id: 'm1', turn_id: 'tq', role: 'user', content: 'x', attachments: [], created_at: '2026-01-01T00:00:00Z', sent_via: sentVia }] },
    } as never)
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    const found = [...document.querySelectorAll('.composer-lock button')].some((b) => b.textContent === LABEL)
    assert.equal(found, shown)
  })
}

for (const [name, status, sentVia, supported, shown] of [
  ['進行中回合的補充', 'in_flight', 'supplement', true, true],
  ['回合已結束', 'completed', 'supplement', true, false],
  ['不是補充', 'in_flight', null, true, false],
  ['不支援 send-now', 'in_flight', 'supplement', false, false],
] as const) {
  test(`補充訊息旁的插隊鈕：${name}`, async () => {
    fakeApi(() => undefined)
    useStore.setState({
      connected: true, defaultConnected: true, selectedBotId: 'bq',
      bots: [{ id: 'bq', name, kind: 'claude', project_id: 'pq', herdr_session: 'default' }],
      projects: [{ id: 'pq', path: '/tmp/q', host: 'local' }],
      runs: { bq: toRun({ id: 'rq', bot_id: 'bq', state: 'running', agent_status: 'working', send_now_supported: supported }) },
      turns: { bq: { tq: { id: 'tq', run_id: 'rq', status, delivery: 'ok' } } },
      drafts: { 'bot:bq': 'draft' },
      messages: { bq: [{ id: 'm1', bot_id: 'bq', turn_id: 'tq', role: 'user', content: 'x', attachments: [], created_at: '2026-01-01T00:00:00Z', sent_via: sentVia }] },
    } as never)
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    const found = [...document.querySelectorAll('.msg-meta button, .msg button, .bubble button')].some((b) => b.textContent === '插隊')
    assert.equal(found, shown)
  })
}
