import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from './store.ts'
import { rewindAndRefill } from './rewindAction.ts'

function stubFetch(status: number, body: unknown) {
  const calls: { url: string; method: string; body: unknown }[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    calls.push({ url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined })
    return { ok: status >= 200 && status < 300, status, statusText: String(status), text: async () => JSON.stringify(body) } as unknown as Response
  }) as unknown as typeof fetch
  return calls
}

test('倒回成功：POST /bots/:id/rewind 帶 message_id，原文接回輸入框最前面（原本打到一半的字留著）', async () => {
  useStore.setState({ drafts: { 'bot:b1': '打到一半' }, notices: [] })
  const calls = stubFetch(200, { rewound: true, text: '問錯的那一句', hidden: 2, pane_cleared: true })
  assert.equal(await rewindAndRefill('b1', 'm-9'), true)
  const post = calls.find((c) => c.method === 'POST')
  assert.ok(post, JSON.stringify(calls))
  assert.equal(post.url, '/api/bots/b1/rewind')
  assert.deepEqual(post.body, { message_id: 'm-9' })
  assert.equal(useStore.getState().drafts['bot:b1'], '問錯的那一句\n打到一半')
  assert.ok(useStore.getState().notices.some((n) => n.kind === 'info' && /原文放回輸入框/.test(n.text)))
})

test('倒回失敗：輸入框不動，通知帶 daemon 的原因', async () => {
  useStore.setState({ drafts: { 'bot:b1': '打到一半' }, notices: [] })
  stubFetch(409, { error: 'conflict', reason: 'not_idle', message: '它正在忙，等這一回合結束再倒回。' })
  assert.equal(await rewindAndRefill('b1', 'm-9'), false)
  assert.equal(useStore.getState().drafts['bot:b1'], '打到一半')
  const n = useStore.getState().notices.find((x) => x.kind === 'error')
  assert.ok(n)
  assert.match(n.text, /它正在忙/)
})

test('終端輸入列沒清掉：照樣回填網頁輸入框，但通知講清楚要去終端清', async () => {
  useStore.setState({ drafts: {}, notices: [] })
  stubFetch(200, { rewound: true, text: '那一句', hidden: 1, pane_cleared: false })
  assert.equal(await rewindAndRefill('b1', 'm-1'), true)
  assert.equal(useStore.getState().drafts['bot:b1'], '那一句')
  assert.ok(useStore.getState().notices.some((n) => /終端的輸入列裡還留著/.test(n.text)))
})
