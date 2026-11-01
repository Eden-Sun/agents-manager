import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from './store.ts'

// claude-api（#1944）：daemon 只收純文字 prompt；送出時拿掉插隊／排隊／自動啟動旗標，停著先 start。
test('claude-api 的 sendPrompt 停著先 start，prompt body 不帶不支援的旗標', async () => {
  const calls: { url: string; method: string; body: unknown }[] = []
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    calls.push({ url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined })
    const body = String(input).endsWith('/start') ? { run_id: 'r1' } : String(input).endsWith('/prompt') ? { turn_id: 't1', delivery: 'pending' } : {}
    return { ok: true, status: 200, statusText: '200', text: async () => JSON.stringify(body) } as unknown as Response
  }) as unknown as typeof fetch
  useStore.setState({ bots: [{ id: 'b1', kind: 'claude-api' }], runs: {}, notices: [] } as never)
  await useStore.getState().sendPrompt('b1', 'hi', [], true, true, undefined, true)
  const posts = calls.filter((c) => c.method === 'POST')
  assert.equal(posts[0]?.url, '/api/bots/b1/start', JSON.stringify(calls))
  const prompt = posts.find((c) => c.url === '/api/bots/b1/prompt')
  assert.ok(prompt, JSON.stringify(calls))
  assert.deepEqual(Object.keys(prompt.body as object).sort(), ['client_request_id', 'text'])
})
