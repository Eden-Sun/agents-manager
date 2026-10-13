/** 「登入好了，重試」的提示文字：上一則使用者訊息截 60 字，截點落在 emoji 代理對中間時不留落單的代理字元。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { Message } from '../api/types'
import { AuthLoginAction } from './AuthLoginAction'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/

function message(over: Partial<Message>): Message {
  return {
    id: 'm1', conversation_id: 'c1', turn_id: 't1', bot_id: 'b1', role: 'user', content: '', source: 'web',
    incomplete: false, group_id: null, attachments: [], relay_from: null, terminal_snapshot: null,
    created_at: '2026-10-10T00:00:00.000Z', ...over,
  } as Message
}

test('登入好了，重試的提示：上一則截 60 字，截點落在 emoji 代理對中間時不留落單的代理字元', async () => {
  fakeApi(() => undefined)
  // 59 個 a 加一顆 😀：UTF-16 的第 60 個單位只剩 😀 的高位代理。
  const user = message({ id: 'u1', role: 'user', content: `${'a'.repeat(59)}😀bc` })
  const authFail = message({ id: 'sys1', role: 'system', content: '（帳號或授權）claude 沒登入', created_at: '2026-10-10T00:00:01.000Z' })
  useStore.setState({
    bots: [{ id: 'b1', name: 'agent', project_id: 'p1', kind: 'claude', identity: null, model: null }],
    messages: { b1: [user, authFail] },
    runs: { b1: { agent_status: 'idle' } },
  } as never)
  await mount(<AuthLoginAction msg={authFail} />)
  const button = [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((candidate) => candidate.textContent?.trim() === '登入好了，重試')!
  assert.equal(button.title, `重新偵測 預設帳號，登入了就重送：${'a'.repeat(59)}😀`)
  assert.doesNotMatch(button.title, LONE_SURROGATE, '不能留下落單的代理字元')
})
