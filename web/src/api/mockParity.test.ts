/**
 * mock 的錯誤碼要跟 daemon（docs/API.md）一樣：用 `VITE_MOCK=1` 驗過的流程，在真環境碰到的是同一組 `reason`。
 * 以前 mock 拿中文句子當 `reason`，子 agent 的 start／restart 在 mock 裡照開，真 daemon 一律 409。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

interface BotRow {
  id: string
  name: string
  parent_bot_id: string | null
  managed_by: string
  run: { id: string; state: string } | null
}
const bots = async (m: MockTransport) =>
  ((await m.request('GET', '/state')) as { projects: { bots: BotRow[] }[] }).projects.flatMap((p) => p.bots)

async function conflict(p: Promise<unknown>): Promise<ApiError> {
  try {
    await p
  } catch (e) {
    if (e instanceof ApiError) return e
    throw e
  }
  assert.fail('預期 ApiError，但請求成功了')
}

test('子 agent 的 start／restart：409 child_restart_forbidden，帶 parent_bot_id 與人話 message（API.md §4、§10.3）', async () => {
  const m = new MockTransport()
  const child = (await bots(m)).find((b) => b.parent_bot_id)
  assert.ok(child, 'mock 的種子資料要有一顆子 agent')
  for (const action of ['start', 'restart']) {
    const e = await conflict(m.request('POST', `/bots/${child.id}/${action}`))
    assert.equal(e.status, 409, action)
    assert.equal(e.body.reason, 'child_restart_forbidden', action)
    assert.equal(e.body.parent_bot_id, child.parent_bot_id, action)
    assert.equal(typeof e.body.message, 'string', `${action}：daemon 附了一句人話，畫面靠它`)
  }
})

test('已經有 active Run 時 start：409，reason 照 API.md 是 "active run already exists"，帶 run_id', async () => {
  const m = new MockTransport()
  const top = (await bots(m)).find((b) => !b.parent_bot_id && !b.run)
  assert.ok(top, 'mock 要有一顆沒在跑的頂層 bot')
  const first = (await m.request('POST', `/bots/${top.id}/start`)) as { run_id: string }
  const e = await conflict(m.request('POST', `/bots/${top.id}/start`))
  assert.equal(e.status, 409)
  assert.equal(e.body.reason, 'active run already exists')
  assert.equal(e.body.run_id, first.run_id)
})
