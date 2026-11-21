/**
 * 使用者訊息旁的承諾帳本進度列（SPEC 帳本第 1 階段 T5）：一訊三事的摘要、展開、補漏／取消的 PATCH／POST，
 * 以及 `ledger_updated` 到達時重新拉這一則。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, LedgerItem, Message } from '../api/types'
import { MessageLedgerBar } from './MessageLedgerBar'
import { ledgerSummary } from '../lib/ledgerSummary'

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
  useStore.setState({ bots: [bot], busy: {} } as never)
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

test('摘要：一訊三事顯示 已答／待跟進／待釐清 的計數（不含已併入）', () => {
  assert.deepEqual(ledgerSummary([...THREE, item({ id: 'x', status: 'merged' })]), { answered: 1, following: 1, clarifying: 1 })
  setup()
  return (async () => {
    const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
    const text = el.querySelector('.ledger-bar-summary')?.textContent ?? ''
    assert.match(text, /3 件：已答 1／待跟進 1／待釐清 1/)
  })()
})

test('只有一筆待分流：顯示「待分流 1」', async () => {
  setup()
  const el = await mount(<MessageLedgerBar msg={msg([item({ status: 'untriaged' })])} />)
  assert.match(el.querySelector('.ledger-bar-summary')?.textContent ?? '', /待分流 1/)
})

test('沒有附掛的訊息不畫進度列、也不發請求', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(undefined)} />)
  assert.equal(el.querySelector('.ledger-bar'), null)
  assert.equal(requests.length, 0)
})

test('展開：逐項列出 L 編號、狀態、承辦人與操作；初次沒有附掛時不發請求', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  const rows = [...el.querySelectorAll('.ledger-item')]
  assert.equal(rows.length, 3)
  assert.match(rows[1].textContent ?? '', /L2/)
  assert.match(rows[1].textContent ?? '', /把檢查腳本改成 60 秒/)
  assert.match(rows[0].textContent ?? '', /agy-bot/, '承辦人顯示 bot 名字')
  assert.equal(requests.length, 0, '有附掛就不發請求')
})

test('取消：PATCH op=cancel 帶 expected_version；補漏：POST 帶 source_message_id', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  const cancel = [...el.querySelectorAll<HTMLButtonElement>('.ledger-item')[1].querySelectorAll('button')].find((b) => b.textContent === '取消')!
  await click(cancel)
  const patch = requests.find((r) => r.method === 'PATCH')!
  assert.match(patch.path, /\/ledger\/li2$/)
  assert.deepEqual(patch.body, { op: 'cancel', expected_version: 1, close_reason: 'cancelled by user' })

  const add = [...el.querySelectorAll<HTMLButtonElement>('.ledger-actions button')].find((b) => b.textContent === '＋補漏')!
  await click(add)
  const input = el.querySelector<HTMLInputElement>('.ledger-inline input')!
  await act(() => setInput(input, '補漏的一件'))
  const confirm = [...el.querySelectorAll<HTMLButtonElement>('.ledger-inline button')].find((b) => b.textContent === '新增')!
  await click(confirm)
  const post = requests.find((r) => r.method === 'POST')!
  assert.deepEqual(post.body, { source_message_id: 'msg_3things', title: '補漏的一件', status: 'following' })
})

test('合併：填入目標短碼（L3）送 PATCH op=merge', async () => {
  const requests = setup()
  const el = await mount(<MessageLedgerBar msg={msg(THREE)} />)
  await click(el.querySelector('.ledger-bar-summary')!)
  const merge = [...el.querySelectorAll<HTMLElement>('.ledger-item')[0].querySelectorAll('button')].find((b) => b.textContent === '合併')!
  await click(merge)
  const input = el.querySelector<HTMLInputElement>('.ledger-inline input')!
  await act(() => setInput(input, 'l3'))
  const confirm = [...el.querySelectorAll<HTMLButtonElement>('.ledger-inline button')].find((b) => b.textContent === '確定合併')!
  await click(confirm)
  const patch = requests.find((r) => r.method === 'PATCH')!
  assert.deepEqual(patch.body, { op: 'merge', expected_version: 3, merged_into_id: 'li3' })
})

test('ledger_updated 到達（rev 變動）才重新拉這一則', async () => {
  const requests = setup()
  await mount(<MessageLedgerBar msg={msg(THREE)} />)
  assert.equal(requests.length, 0)
  await act(() => useStore.setState((s) => ({ ledgerRev: { ...s.ledgerRev, msg_3things: 1 } })) as never)
  await new Promise((r) => setTimeout(r, 10))
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.includes('source_message_id=msg_3things')), JSON.stringify(requests))
})
