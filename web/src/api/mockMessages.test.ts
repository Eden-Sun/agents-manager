/**
 * mock 的單 bot 訊息端點要跟 `daemon/src/api.rs::messages` 同形（docs/FRONTEND.md）。
 * 少掉 `limit`／`before`／`turn_id`／`role` 的話，issue #25 的往前翻頁與群組未讀的回合確認
 * 在 `VITE_MOCK=1` 下走的都不是真 daemon 那條路：畫面全對，推上去才炸。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

interface Page {
  messages: { id: string; role: string; turn_id: string | null; content: string }[]
  has_more: boolean
}

const mock = new MockTransport()
const firstBotId = async () => {
  const st = (await mock.request('GET', '/state')) as { projects: { bots: { id: string }[] }[] }
  return st.projects[0].bots[0].id
}
const page = async (botId: string, qs = '') =>
  (await mock.request('GET', `/bots/${botId}/messages${qs}`)) as Page

test('預設一頁就滿了：`has_more` 是真的算出來的，不是硬編 false', async () => {
  const id = await firstBotId()
  const first = await page(id, '?limit=100')
  assert.equal(first.messages.length, 100)
  assert.equal(first.has_more, true)
})

test('`before` 往前翻，翻到的是更早的那一段且不重疊', async () => {
  const id = await firstBotId()
  const first = await page(id, '?limit=10')
  const older = await page(id, `?limit=10&before=${first.messages[0].id}`)
  assert.equal(older.messages.length, 10)
  assert.equal(older.has_more, true)
  const seen = new Set(first.messages.map((m) => m.id))
  assert.ok(older.messages.every((m) => !seen.has(m.id)), '翻頁不能回同一批訊息')
  // 再用最舊那一則往前翻，最後一頁 `has_more` 要收斂成 false。
  let cursor = older.messages[0].id
  let guard = 0
  let last = older
  while (last.has_more && guard++ < 100) {
    last = await page(id, `?limit=50&before=${cursor}`)
    if (last.messages.length > 0) cursor = last.messages[0].id
  }
  assert.equal(last.has_more, false)
})

test('`turn_id` 與 `role` 是先過濾再分頁（群組未讀確認就靠這個）', async () => {
  const id = await firstBotId()
  const all = await page(id, '?limit=500')
  const turnId = all.messages.find((m) => m.turn_id)?.turn_id
  assert.ok(turnId, 'seed 要有帶 turn 的訊息')
  const one = await page(id, `?turn_id=${turnId}&role=user`)
  assert.ok(one.messages.length > 0)
  assert.ok(one.messages.every((m) => m.turn_id === turnId && m.role === 'user'))
  assert.equal((await page(id, '?turn_id=nope')).messages.length, 0, '查不到的回合是空清單，不是整段歷史')
})

test('非法 role 回 400，不是靜默忽略', async () => {
  const id = await firstBotId()
  await assert.rejects(
    () => page(id, '?role=bot'),
    (e: unknown) => e instanceof ApiError && e.status === 400,
  )
})
