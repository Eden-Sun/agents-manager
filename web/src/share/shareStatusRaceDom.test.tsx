import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient, ShareEvents } from './shareApi'
import { mockShareClient } from './shareMock'
import type { ShareMessage, SharePage } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

const msg = (id: string, role: 'user' | 'assistant', n: number): ShareMessage => ({
  id,
  role,
  text: id,
  created_at: new Date(Date.UTC(2026, 9, 13, 0, n)).toISOString(),
  attachments: [],
})
const page = (status: SharePage['status'], ms: ShareMessage[]): SharePage => ({ bot_name: 'b', status, has_more: false, messages: ms })

function gatedClient(): { client: ShareClient; events: () => ShareEvents; pending: ((p: SharePage) => void)[] } {
  let ev!: ShareEvents
  const pending: ((p: SharePage) => void)[] = []
  let calls = 0
  const base = mockShareClient(TOKEN)
  const client: ShareClient = {
    ...base,
    messages: () => {
      calls++
      if (calls === 1) return Promise.resolve(page('idle', [msg('base', 'user', 0)]))
      return new Promise((resolve) => pending.push(resolve))
    },
    subscribe: (events) => {
      ev = events
      return () => {}
    },
  }
  return { client, events: () => ev, pending }
}

async function triggerPendingGet(): Promise<void> {
  await act(async () => document.dispatchEvent(new Event('visibilitychange')))
}

const sendButton = () => document.querySelector<HTMLButtonElement>('.sh-send')!
const thinking = () => document.querySelector('.sh-thinking')

async function typeDraft(text: string): Promise<void> {
  const ta = document.querySelector('textarea')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
    set.call(ta, text)
    ta.dispatchEvent(new Event('input', { bubbles: true }))
  })
}

test('HTTP 快照較早發出、較晚回來，且期間 SSE 已報 idle：回覆照常顯示，送出鈕不被鎖回去', async () => {
  const { client, events, pending } = gatedClient()
  await mount(<ShareApp client={client} />)
  await settle(50)
  await triggerPendingGet()
  assert.equal(pending.length, 1, '回到前景發出一趟還沒回的 messages GET')

  const answer = msg('answer-1', 'assistant', 1)
  await act(async () => events().onMessage(answer))
  await act(async () => events().onStatus('idle'))
  await act(async () => pending[0](page('working', [msg('base', 'user', 0), answer])))

  assert.ok(document.body.textContent!.includes('answer-1'), '回覆要顯示')
  assert.equal(thinking() !== null, false, '已 idle，不能再顯示思考中')
  await typeDraft('下一題')
  assert.equal(sendButton().disabled, false, '已 idle，送出鈕要可以按')
})

test('HTTP 快照較早發出、較晚回來，且期間 SSE 已報 working：舊的 idle 快照不能放開送出鈕', async () => {
  const { client, events, pending } = gatedClient()
  await mount(<ShareApp client={client} />)
  await settle(50)
  await triggerPendingGet()
  assert.equal(pending.length, 1)

  await act(async () => events().onStatus('working'))
  await typeDraft('下一題')
  assert.equal(sendButton().disabled, true, 'SSE 說 working 時送出鈕要停用')
  assert.ok(thinking(), '要顯示思考中')

  await act(async () => pending[0](page('idle', [msg('base', 'user', 0)])))
  assert.equal(sendButton().disabled, true, '舊 idle 快照不能放開送出鈕')
  assert.ok(thinking(), '思考中仍要顯示')
})

test('沒有 SSE 狀態插入時，HTTP 快照的 status 照常採用（working 會鎖住送出鈕）', async () => {
  const { client, pending } = gatedClient()
  await mount(<ShareApp client={client} />)
  await settle(50)
  await triggerPendingGet()
  assert.equal(pending.length, 1)

  await act(async () => pending[0](page('working', [msg('base', 'user', 0)])))
  await typeDraft('下一題')
  assert.equal(sendButton().disabled, true)
  assert.ok(thinking())
})
