/**
 * #1899：撞限（`limit_hit`）出現、清掉、或 until 改變時重抓一次 /api/state（`bot.limit_hit` 在那裡）；
 * 一般讀數不重抓；沒有先前快取的 key 不因第一次收到就重抓；連續多筆被合併。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { KindQuota } from '../api/types.ts'

const settle = () => new Promise((r) => setTimeout(r, 0))
const stateFetches = () => requests.filter((r) => r.path === '/api/state').length

const win = (used: number) => ({ used_pct: used, resets_at: null, observed_at: null, low: false, critical: false })
const quota = (used: number, limit_hit: KindQuota['limit_hit'] = null): Record<string, unknown> => ({
  five_hour: win(used),
  seven_day: null,
  fable: null,
  reset_credits: null,
  limit_hit,
  plan: 'plus',
  updated_at: 't',
  stale: false,
  host: 'local',
})
const hit = (until: string | null, message = "You've hit your usage limit") => ({ message, until, at: '2026-10-22T10:00:00Z', bucket: null })

function seed(codex: Record<string, unknown> | undefined) {
  reset()
  useStore.setState({ quota: codex ? { codex: codex as never } : {} })
}

test('limit_hit 新出現、被清掉、until 改變：各重抓一次', async () => {
  seed(quota(40))
  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(40, hit('2026-10-29T11:24:00Z')) } })
  await settle()
  assert.equal(stateFetches(), 1, '新出現')

  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(40, hit('2026-10-30T09:00:00Z')) } })
  await settle()
  assert.equal(stateFetches(), 2, 'until 改了')

  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(40) } })
  await settle()
  assert.equal(stateFetches(), 3, '被清掉')
})

test('一般讀數變了、撞限沒變：不重抓', async () => {
  seed(quota(40, hit('2026-10-29T11:24:00Z')))
  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(55, hit('2026-10-29T11:24:00Z')) } })
  await settle()
  assert.equal(stateFetches(), 0)
})

test('沒有先前快取的 key：第一次收到就算有撞限也不重抓', async () => {
  seed(undefined)
  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(40, hit('2026-10-29T11:24:00Z')) } })
  await settle()
  assert.equal(stateFetches(), 0)
})

test('整包 {kinds} 裡只有一筆撞限變了：只重抓一次；遠端 key 用完整的 <host>/ 前綴比對', async () => {
  seed(quota(40))
  useStore.setState({ quota: { ...useStore.getState().quota, 'm4p/codex': quota(10) as never } })
  dispatchFrameForTest({
    type: 'quota_updated',
    data: {
      kinds: {
        codex: quota(45),
        'm4p/codex': quota(12, hit('2026-10-29T11:24:00Z')),
      },
    },
  })
  await settle()
  assert.equal(stateFetches(), 1, '只有 m4p/codex 變了，只重抓一次')
})

test('遠端 key 不會因為短 kind 相同而誤判：只看 m4p/codex 的前綴，local codex 沒變就不重抓', async () => {
  seed(quota(40, hit('2026-10-29T11:24:00Z')))
  useStore.setState({ quota: { ...useStore.getState().quota, 'm4p/codex': quota(10) as never } })
  // 只更新遠端那把，且撞限沒變：不該因為 local 的 codex 有撞限就重抓。
  dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'm4p/codex', host: 'm4p', quota: quota(11) } })
  await settle()
  assert.equal(stateFetches(), 0)
})

test('短時間連續多筆撞限變化：合併成少量重抓（single-flight）', async () => {
  seed(quota(40))
  for (const until of ['2026-10-29T11:00:00Z', '2026-10-29T12:00:00Z', '2026-10-29T13:00:00Z']) {
    dispatchFrameForTest({ type: 'quota_updated', data: { kind: 'codex', host: 'local', quota: quota(40, hit(until)) } })
  }
  await settle()
  await settle()
  assert.ok(stateFetches() < 3, `三筆連續變化應被合併，實際抓了 ${stateFetches()} 次`)
  assert.ok(stateFetches() >= 1)
})
