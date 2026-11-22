import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, LedgerItem, Message } from '../api/types'
import { MessageLedgerBar } from './MessageLedgerBar'

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
  source_message_id: 'msg_3things',
  ordinal: 0,
  title: '為什麼剛剛重啟',
  status: 'answered',
  owner_bot_id: 'b1',
  deliverable: '',
  next_step: '',
  wait_for: null,
  due_at: null,
  merged_into_id: null,
  close_reason: null,
  version: 3,
  updated_at: '2026-11-21T00:00:00Z',
  ...over,
})

const THREE: LedgerItem[] = [
  item({ id: 'li1', code: 'L1', code_no: 1, status: 'answered', title: '為什麼剛剛重啟' }),
  item({ id: 'li2', code: 'L2', code_no: 2, ordinal: 1, status: 'following', title: '把檢查腳本改成 60 秒', version: 1 }),
  item({ id: 'li3', code: 'L3', code_no: 3, ordinal: 2, status: 'clarifying', title: '確認 Mac 節點連線', version: 2 }),
]

const msg = (ledger?: LedgerItem[]): Message =>
  ({ id: 'msg_3things', bot_id: 'b1', role: 'user', content: '三件事', ledger_items: ledger, conversation_id: 'c', turn_id: null, source: 'web', incomplete: false, group_id: null, attachments: [], relay_from: null, terminal_snapshot: null, created_at: '', seq: 1 }) as unknown as Message

function setup() {
  const requests: { method: string; path: string; body: unknown }[] = []
  const bot = { id: 'b1', name: 'agy-bot', project_id: 'p1', kind: 'agy', model: null, effort: null, fast: false, identity: null, needs_restart: false } as unknown as Bot
  useStore.setState({ bots: [bot], busy: {}, ledgerRev: {} } as never)
  mockApi({
    request: async (method: string, path: string, body?: unknown) => {
      requests.push({ method, path, body })
      if (method === 'GET' && path.endsWith('/ledger')) return { items: THREE }
      if (method === 'GET' && path.includes('/ledger?source_message_id=')) return { items: THREE }
      if (method === 'PATCH') return { ...THREE[1], version: THREE[1].version + 1, status: 'cancelled' }
      if (method === 'POST') return { items: [item({ id: 'li4', code: 'L4', code_no: 4, ordinal: 3, status: 'following', title: '補漏的一件' })] }
      return null
    },
  } as never)
  return requests
}

function setInput(el: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!
  setter.call(el, value)
  el.dispatchEvent(new Event('input', { bubbles: true }))
}

const btn = (root: ParentNode, text: string) => [...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text)!

test('摘要：白話的三種狀態，不講帳本術語；不列已取消與已併入', async () => {
  setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  assert.equal(el.querySelector('.ledger-bar-summary')?.textContent?.trim(), '📋 追蹤 3 項：待你驗收 1・需要你回覆 1・進行中 1')
  const done = [item({ id: 'd1', status: 'done' }), item({ id: 'd2', code: 'L2', code_no: 2, status: 'cancelled' }), item({ id: 'd3', code: 'L3', code_no: 3, status: 'merged' })]
  const awaiting = [item({ id: 'a1', status: 'done' }), item({ id: 'a2', code: 'L2', code_no: 2, status: 'answered' })]
  const el3 = await mount(<MessageLedgerBar msg={msg(awaiting)} />)
  assert.equal(el3.querySelector('.ledger-bar-summary')?.textContent?.trim(), '📋 追蹤 2 項：待你驗收 1・已完成 1')
  const el2 = await mount(<MessageLedgerBar msg={msg(done)} />)
  assert.equal(el2.querySelector('.ledger-bar-summary')?.textContent?.trim(), '✓ 追蹤完成')
})

test('全部尚未整理：「📋 已加入追蹤，等管理者整理」', async () => {
  setup()
  const el = await mount(<MessageLedgerBar msg={msg([item({ status: 'untriaged' })])} />)
  assert.equal(el.querySelector('.ledger-bar-summary')?.textContent?.trim(), '📋 已加入追蹤，等管理者整理')
})

test('沒有附掛的訊息不畫進度列、也不發請求', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(undefined)} />)
  assert.equal(el.querySelector('.ledger-bar'), null)
  assert.equal(requests.length, 0)
})

test('面板：每項一行（標題＋白話狀態＋下一步），操作收在「⋯」裡；已回答的沒有「⋯」，沒有合併', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  assert.ok(el.querySelector('[role="dialog"]'), '面板開著')
  const rows = [...el.querySelectorAll('.ledger-row')]
  assert.equal(rows.length, 3)
  assert.deepEqual(rows.map((r) => r.querySelector('.ledger-code')?.textContent), ['L1', 'L2', 'L3'])
  assert.match(rows[1].textContent ?? '', /把檢查腳本改成 60 秒/)
  assert.match(rows[2].textContent ?? '', /需要你回覆/)
  assert.equal(rows[0].querySelector('.ledger-more'), null, '已回答的不再有操作')
  assert.ok(rows[1].querySelector('.ledger-more'))
  assert.equal(el.textContent?.includes('合併'), false, '合併不在使用者端')
  assert.equal(requests.length, 0, '有附掛就不發請求')
})

test('「⋯」→「不用做了」：PATCH op=cancel 帶 expected_version；「已經好了」：PATCH status=done', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  await click(el.querySelectorAll<HTMLButtonElement>('.ledger-row')[1].querySelector('.ledger-more')!)
  await click(btn(el, '不用做了'))
  const cancel = requests.find((r) => r.method === 'PATCH')!
  assert.match(cancel.path, /\/ledger\/li2$/)
  assert.deepEqual(cancel.body, { op: 'cancel', expected_version: 1, close_reason: 'cancelled by user' })

  await click(el.querySelectorAll<HTMLButtonElement>('.ledger-row')[1].querySelector('.ledger-more')!)
  await click(btn(el, '已經好了'))
  const done = requests.filter((r) => r.method === 'PATCH')[1]
  assert.match(done.path, /\/ledger\/li3$/)
  assert.deepEqual(done.body, { op: 'update', expected_version: 2, status: 'done' })
})

test('底部「＋ 漏了什麼？補一項」：一句話 → POST 帶 source_message_id', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  await click(btn(el, '＋ 漏了什麼？補一項'))
  const input = el.querySelector<HTMLInputElement>('.ledger-add input')!
  await act(() => setInput(input, '補漏的一件'))
  await click(btn(el, '新增'))
  const post = requests.find((r) => r.method === 'POST')!
  assert.deepEqual(post.body, { source_message_id: 'msg_3things', title: '補漏的一件', status: 'following' })
})

test('尚未整理：面板只說明＋「補一項」＋「這則不用追蹤」（取消這則底下待整理的項目）', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg([item({ status: 'untriaged', version: 2 })])} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  assert.equal(el.querySelectorAll('.ledger-row').length, 0)
  assert.match(el.querySelector('.ledger-hint')?.textContent ?? '', /管理者還沒整理/)
  await click(btn(el, '這則不用追蹤'))
  const patch = requests.find((r) => r.method === 'PATCH')!
  assert.deepEqual(patch.body, { op: 'cancel', expected_version: 2, close_reason: 'cancelled by user' })
})

test('ledger_updated 到達（rev 變動）才重新拉這一則', async () => {
  const requests = setup()
  await mount(<MessageLedgerBar msg={msg(THREE)} />)
  assert.equal(requests.length, 0)
  await act(() => useStore.setState((s) => ({ ledgerRev: { ...s.ledgerRev, msg_3things: 1 } })) as never)
  await new Promise((r) => setTimeout(r, 10))
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.includes('source_message_id=msg_3things')), JSON.stringify(requests))
})

test('使用者訊息的「⋯」→「加入追蹤」：POST 帶 source_message_id、前 80 字標題、status=untriaged', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(undefined)} />)
  await click(el.querySelector('.ledger-add-row .ledger-more')!)
  await click(btn(el, '加入追蹤'))
  const post = requests.find((r) => r.method === 'POST')!
  assert.deepEqual(post.body, { source_message_id: 'msg_3things', title: '三件事', status: 'untriaged' })
})

test('已有追蹤項的訊息：選單顯示「已在追蹤」且不可按', async () => {
  setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-add-row .ledger-more')!)
  const already = btn(el, '已在追蹤')
  assert.ok(already.disabled)
  assert.equal(btn(el, '加入追蹤'), undefined)
})

test('標題：只取第一行、最多 80 字', async () => {
  const requests = setup()
  const long = '一'.repeat(120) + '\n第二行'
  const el = await mount(<MessageLedgerBar msg={{ ...msg(undefined), content: long } as Message} />)
  await click(el.querySelector('.ledger-add-row .ledger-more')!)
  await click(btn(el, '加入追蹤'))
  const post = requests.find((r) => r.method === 'POST')!
  assert.equal((post.body as { title: string }).title, '一'.repeat(80))
})

test('待驗收：完成／不知道兩顆按鈕；不知道 PATCH 回到進行中、換成複查指示、清掉等待條件、到期時間設為現在', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  const row = el.querySelectorAll<HTMLElement>('.ledger-row')[0]
  assert.equal(row.querySelector('.ledger-more'), null, '待驗收沒有 ⋯')
  assert.equal(btn(row, '還沒好'), undefined, '沒有「還沒好」')
  await click(btn(row, '完成'))
  assert.deepEqual(requests.find((r) => r.method === 'PATCH')!.body, { op: 'update', expected_version: 3, status: 'done' })
  await click(btn(row, '不知道'))
  const notSure = requests.filter((r) => r.method === 'PATCH')[1]
  assert.match(notSure.path, /\/ledger\/li1$/)
  const body = notSure.body as { op: string; expected_version: number; status: string; next_step: string; wait_for: string; due_at: string }
  assert.deepEqual([body.op, body.expected_version, body.status, body.wait_for], ['update', 3, 'following', ''])
  assert.equal(body.next_step, '使用者不確定是否完成：請複查完成度並附驗證證據（commit、測試、截圖或實測結果）後再交件')
  assert.ok(!Number.isNaN(Date.parse(body.due_at)) && Date.parse(body.due_at) <= Date.now() + 1000, 'due_at 是現在')
})
