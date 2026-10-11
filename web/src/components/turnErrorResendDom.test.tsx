/** 回合中斷後重送上一則要一併送出原本的附件 id。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { TurnErrorBadge } from './TurnErrorBadge'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const attachment = (id: string) => ({ id, name: `${id}.png`, mime: 'image/png', size: 10, path: `/tmp/${id}.png` })
const userMessage = (content: string) => ({
  id: 'u1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content, source: 'web',
  incomplete: false, group_id: null, attachments: [attachment('a1'), attachment('a2')], relay_from: null,
  terminal_snapshot: null, created_at: '2026-10-10T00:00:00.000Z',
})

function setup(content: string) {
  const requests = fakeApi((req: FakeRequest) => {
    if (req.method === 'POST' && req.path.endsWith('/bots/b1/prompt')) {
      return { turn_id: 't2', message_id: 'u2', delivery: 'pending', send_now: null }
    }
    return undefined
  })
  useStore.setState({
    bots: [{ id: 'b1', name: 'agent', project_id: 'p1', kind: 'claude', identity: null, model: null }],
    messages: { b1: [userMessage(content)] },
    runs: { b1: { turn_error: 'API Error: Connection error.', agent_status: 'idle' } },
  } as never)
  return requests
}

async function resend() {
  await mount(<TurnErrorBadge botId="b1" />)
  const badge = document.querySelector<HTMLButtonElement>('.turn-error-badge')!
  await act(() => badge.click())
  const button = [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((candidate) => candidate.textContent?.trim() === '重送上一則')!
  await act(() => button.click())
  await settle(50)
}

test('重送帶文字與附件的上一則：POST 帶回原附件 id', async () => {
  const requests = setup('看圖')
  await resend()
  const prompt = requests.find((request) => request.method === 'POST' && request.path.endsWith('/bots/b1/prompt'))
  assert.deepEqual((prompt?.body as { attachments?: string[] } | undefined)?.attachments, ['a1', 'a2'])
})

test('只有附件的上一則仍可重送', async () => {
  const requests = setup('')
  await mount(<TurnErrorBadge botId="b1" />)
  await act(() => document.querySelector<HTMLButtonElement>('.turn-error-badge')!.click())
  const button = [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((candidate) => candidate.textContent?.trim() === '重送上一則')!
  assert.equal(button.disabled, false)
  await act(() => button.click())
  await settle(50)
  const prompt = requests.find((request) => request.method === 'POST' && request.path.endsWith('/bots/b1/prompt'))
  assert.deepEqual((prompt?.body as { attachments?: string[] } | undefined)?.attachments, ['a1', 'a2'])
})
