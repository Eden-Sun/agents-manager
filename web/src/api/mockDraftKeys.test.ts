/** mock 的 `PUT /drafts/{key}` 要收 host shell 面板的指令草稿 `shell:<host>/<pane id>`（API.md「輸入框草稿」，#758），不回 400。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'

const mock = new MockTransport()

test('shell 指令草稿 key 寫得進去、讀得回來（rev 從 1 開始）', async () => {
  const key = encodeURIComponent('shell:local/p_1')
  const out = (await mock.request('PUT', `/drafts/${key}`, { text: 'ls -la', client_id: 'c1' })) as { key: string; rev: number }
  assert.equal(out.key, 'shell:local/p_1')
  assert.equal(out.rev, 1)
  const list = (await mock.request('GET', '/drafts')) as { drafts: { key: string; text: string }[] }
  assert.ok(list.drafts.some((d) => d.key === 'shell:local/p_1' && d.text === 'ls -la'))
})

test('shell key 缺 pane 段、或別的前綴照樣 400 bad_draft_key', async () => {
  for (const key of ['shell:local', 'other:local/p_1']) {
    await assert.rejects(
      mock.request('PUT', `/drafts/${encodeURIComponent(key)}`, { text: 'x' }),
      (e: unknown) => (e as { status?: number }).status === 400,
    )
  }
})
