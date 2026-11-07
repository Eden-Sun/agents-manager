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
  ['版本未知（子 agent）仍顯示，由 daemon 讀畫面判斷', '', 'supplement', false, true],
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
  ['版本未知（子 agent）仍顯示，由 daemon 讀畫面判斷', 'in_flight', 'supplement', false, true],
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
    const btn = document.querySelector<HTMLButtonElement>('.msg > .queued-jump')
    assert.equal(Boolean(btn), shown)
    if (btn) {
      // 在泡泡下面、不在 meta 列裡（不撐高 meta）；可存取：有文字、aria-label、tooltip。
      assert.equal(document.querySelector('.msg-meta .queued-jump'), null)
      assert.equal(btn.previousElementSibling?.classList.contains('bubble'), true)
      assert.equal(btn.textContent, '⤴插隊送出')
      assert.match(btn.getAttribute('aria-label') ?? '', /插隊送出/)
      assert.match(btn.title, /打斷目前這一輪/)
    }
  })
}
