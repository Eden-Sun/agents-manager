/**
 * #1899：`limit_hit` 要在兩條真的路徑上活下來——`/api/state`（toBot 正規化）與 WS `bot_status`（逐欄 patch）。
 * 之前 toBot 沒帶這個欄位，DOM 測試直接塞 store 所以沒抓到。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import { toBotLimitHit } from '../api/normalize.ts'
import type { Bot } from '../api/types.ts'

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const HIT = { message: "You've hit your usage limit", until: '2026-10-29T11:24:00Z' }

/** daemon 的 bot 形狀（`/api/state` 裡一顆）。 */
const rawBot = (id: string, parent: string | null, limit_hit: unknown) => ({
  id,
  project_id: 'p1',
  name: id,
  kind: 'codex',
  identity: null,
  herdr_session: null,
  parent_bot_id: parent,
  managed_by: parent ? 'child' : 'user',
  limit_hit,
})

test('toBotLimitHit：物件保留 message 與 until；非物件一律 null', () => {
  assert.deepEqual(toBotLimitHit(HIT), HIT)
  assert.deepEqual(toBotLimitHit({ message: 'x', until: null }), { message: 'x', until: null })
  assert.equal(toBotLimitHit(null), null)
  assert.equal(toBotLimitHit(undefined), null)
  assert.equal(toBotLimitHit('usage limit'), null)
  assert.equal(toBotLimitHit(42), null)
})

test('toBotLimitHit：型別不對整筆給 null，不把任意物件塞進 store；多出來的欄位不帶', () => {
  // message 不是字串（數字、物件、缺欄位）：整筆 null。
  assert.equal(toBotLimitHit({ message: 42, until: HIT.until }), null)
  assert.equal(toBotLimitHit({ message: { text: 'x' }, until: HIT.until }), null)
  assert.equal(toBotLimitHit({ until: HIT.until }), null)
  // until 不是字串也不是 null（數字、物件）：整筆 null，不能退成「沒寫時間」而一直掛著。
  assert.equal(toBotLimitHit({ message: 'x', until: 1761737040 }), null)
  assert.equal(toBotLimitHit({ message: 'x', until: { at: 'now' } }), null)
  // 合法的物件只取 message 與 until，其餘欄位（甚至是巢狀物件）不進 store。
  const parsed = toBotLimitHit({ ...HIT, bucket: 'five_hour', extra: { deep: [1, 2] } })
  assert.deepEqual(parsed, HIT)
  assert.ok(parsed && !('extra' in parsed) && !('bucket' in parsed), '只留 message 與 until')
})

test('/api/state：頂層主力的 limit_hit 經過正規化後保留；child 的 null 與舊 daemon 沒這欄都是 null', async () => {
  reset()
  routeDaemon((r) => {
    if (r.path === '/api/state') {
      const bots = [rawBot('top', null, HIT), rawBot('kid', 'top', null), { ...rawBot('old', null, undefined), limit_hit: undefined }]
      return json({ daemon_seq: 1, projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local', bots }], bots, runs: [], turns: [] })
    }
    return json({})
  })
  await useStore.getState().refreshState()
  const byId = (id: string) => useStore.getState().bots.find((b) => b.id === id) as Bot
  assert.deepEqual(byId('top').limit_hit, HIT, '/api/state 的撞限要活到 store')
  assert.equal(byId('kid').limit_hit, null, 'child 一律 null')
  assert.equal(byId('old').limit_hit, null, '舊 daemon 沒這欄')
})

test('bot_status：帶 limit_hit 就套到那顆 bot；清掉變 null；沒有這個 key 不動；值一樣不換陣列', async () => {
  reset()
  routeDaemon(() => json({}))
  useStore.setState({
    bots: [{ ...rawBot('top', null, null) }, { ...rawBot('other', null, null) }] as never,
    runs: {},
  })
  const bots = () => useStore.getState().bots
  const find = (id: string) => bots().find((b) => b.id === id) as Bot

  dispatchFrameForTest({ type: 'bot_status', data: { bot_id: 'top', host: 'local', connected: true, run: null, limit_hit: HIT } })
  assert.deepEqual(find('top').limit_hit, HIT, '撞限出現')
  assert.equal(find('other').limit_hit, null, '別顆不動')

  const before = bots()
  dispatchFrameForTest({ type: 'bot_status', data: { bot_id: 'top', host: 'local', connected: true, run: null, limit_hit: HIT } })
  assert.equal(bots(), before, '值沒變就不換 bots 陣列')

  dispatchFrameForTest({ type: 'bot_status', data: { bot_id: 'top', host: 'local', connected: true, run: null } })
  assert.deepEqual(find('top').limit_hit, HIT, '沒有 limit_hit 這個 key（舊 daemon）不動')

  dispatchFrameForTest({ type: 'bot_status', data: { bot_id: 'top', host: 'local', connected: true, run: null, limit_hit: null } })
  assert.equal(find('top').limit_hit, null, '撞限被清掉')
})
