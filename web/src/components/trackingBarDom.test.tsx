/**
 * 對話窗上方的「追蹤中」列（TrackingBar，2026-11-21）：只在有未結事項時出現、摘要計數、收合記在 localStorage、
 * 面板排序（需要你回覆 → 已加入追蹤 → 進行中）、點項目捲到來源訊息（不在已載入範圍給提示）、「⋯」操作，以及 `ledger_updated` 重抓。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, LedgerItem, Message } from '../api/types'
import { TrackingBar } from './TrackingBar'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const item = (over: Partial<LedgerItem>): LedgerItem => ({
  id: 'li1',
  code: 'L1',
  code_no: 1,
  project_id: 'p1',
  source_message_id: 'msg_1',
  ordinal: 0,
  title: '第一件',
  status: 'untriaged',
  owner_bot_id: 'b1',
  deliverable: '',
  next_step: '',
  wait_for: null,
  due_at: null,
  merged_into_id: null,
  close_reason: null,
  version: 1,
  updated_at: '2026-11-21T00:00:00Z',
  ...over,
})

const OPEN: LedgerItem[] = [
  item({ id: 'li2', code: 'L2', code_no: 2, status: 'following', title: '把檢查腳本改成 60 秒', next_step: '等 CI', updated_at: '2026-11-21T03:00:00Z' }),
  item({ id: 'li1', code: 'L1', code_no: 1, status: 'untriaged', title: '請查 CI 紅的原因' }),
  item({ id: 'li3', code: 'L3', code_no: 3, status: 'clarifying', title: '確認 Mac 節點連線', source_message_id: 'msg_2', version: 2 }),
]

const bot = { id: 'b1', name: 'agy-bot', project_id: 'p1', kind: 'agy', model: null, effort: null, fast: false, identity: null, needs_restart: false } as unknown as Bot
const message = (id: string, content: string): Message =>
  ({ id, bot_id: 'b1', role: 'user', content, conversation_id: 'c', turn_id: null, source: 'web', incomplete: false, group_id: null, attachments: [], created_at: '2026-11-21T02:30:00Z' }) as unknown as Message

function setup(open: LedgerItem[] = OPEN) {
  const requests: { method: string; path: string; body: unknown }[] = []
  useStore.setState({ bots: [bot], busy: {}, messages: { b1: [message('msg_1', '第一則'), message('msg_2', '第二則')] } } as never)
  mockApi({
    request: async (method: string, path: string, body?: unknown) => {
      requests.push({ method, path, body })
      if (method === 'GET' && path.includes('/ledger?status=open')) return { items: open }
      if (method === 'PATCH') return { ...open[0], version: open[0].version + 1, status: 'cancelled' }
      return null
    },
  } as never)
  return requests
}

const btn = (root: ParentNode, text: string) => [...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text)!

test('沒有未結事項：整列不出現', async () => {
  setup([])
  const el = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  assert.equal(el.querySelector('.track-bar'), null)
})

test('摘要：追蹤中 N 項；有需要你回覆才加上「需要你回覆 M」', async () => {
  setup()
  const el = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  assert.equal(el.querySelector('.track-summary')?.textContent?.trim(), '📋 追蹤中 3 項・需要你回覆 1')
  setup(OPEN.filter((it) => it.status !== 'clarifying'))
  const el2 = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  assert.equal(el2.querySelector('.track-summary')?.textContent?.trim(), '📋 追蹤中 2 項')
})

test('收合：變成單行的「追蹤中」小鈕；再點展開回來', async () => {
  setup()
  const el = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  await click(el.querySelector('.track-collapse')!)
  assert.ok(el.querySelector('.track-pill'), '收合後只剩 pill')
  assert.equal(el.querySelector('.track-summary'), null)
  await click(el.querySelector('.track-pill')!)
  assert.ok(el.querySelector('.track-summary'), '展開回來')
})

test('面板排序：需要你回覆 → 已加入追蹤 → 進行中；每項有短碼、標題、白話狀態、下一步與訊息時間', async () => {
  setup()
  const el = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  await click(el.querySelector('.track-summary')!)
  const rows = [...el.querySelectorAll<HTMLElement>('.ledger-row')]
  assert.deepEqual(rows.map((r) => r.querySelector('.ledger-code')?.textContent), ['L3', 'L1', 'L2'])
  assert.match(rows[0].querySelector('.ledger-chip')?.textContent ?? '', /需要你回覆/)
  assert.match(rows[1].querySelector('.ledger-chip')?.textContent ?? '', /已加入追蹤/)
  assert.match(rows[2].textContent ?? '', /下一步：等 CI/)
  assert.match(rows[2].textContent ?? '', /訊息時間：/)
})

test('點項目＝捲到來源訊息（onJump 收到來源訊息 id），成功後面板收起', async () => {
  setup()
  const jumped: string[] = []
  const el = await mount(<TrackingBar botId="b1" onJump={(id) => (jumped.push(id), true)} />)
  await click(el.querySelector('.track-summary')!)
  await click(el.querySelectorAll<HTMLElement>('.ledger-row')[2].querySelector('.track-jump')!)
  assert.deepEqual(jumped, ['msg_1'], 'L2 來自 msg_1')
  assert.equal(el.querySelector('[role="dialog"]'), null)
})

test('來源訊息不在已載入範圍：不收起，顯示提示', async () => {
  setup()
  const el = await mount(<TrackingBar botId="b1" onJump={() => false} />)
  await click(el.querySelector('.track-summary')!)
  await click(el.querySelectorAll<HTMLElement>('.ledger-row')[0].querySelector('.track-jump')!)
  assert.ok(el.querySelector('[role="dialog"]'), '面板還開著')
  assert.match(el.querySelector('.track-hint')?.textContent ?? '', /還沒載入/)
})

test('「⋯」→「不用做了」：PATCH op=cancel 帶 expected_version，然後重抓', async () => {
  const requests = setup()
  const el = await mount(<TrackingBar botId="b1" onJump={() => true} />)
  await click(el.querySelector('.track-summary')!)
  await click(el.querySelectorAll<HTMLElement>('.ledger-row')[0].querySelector('.ledger-more')!)
  await click(btn(el, '不用做了'))
  const patch = requests.find((r) => r.method === 'PATCH')!
  assert.match(patch.path, /\/ledger\/li3$/)
  assert.deepEqual(patch.body, { op: 'cancel', expected_version: 2, close_reason: 'cancelled by user' })
  assert.ok(requests.filter((r) => r.method === 'GET').length >= 2, '操作後重抓列表')
})

test('ledger_updated（任何變動）到達就重抓 status=open', async () => {
  const requests = setup()
  await mount(<TrackingBar botId="b1" onJump={() => true} />)
  const before = requests.filter((r) => r.method === 'GET').length
  await act(() => useStore.setState((s) => ({ ledgerAnyRev: s.ledgerAnyRev + 1 })) as never)
  await new Promise((r) => setTimeout(r, 10))
  assert.equal(requests.filter((r) => r.method === 'GET').length, before + 1)
})
