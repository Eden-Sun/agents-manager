import test from 'node:test'
import assert from 'node:assert/strict'
import type { KindQuota, QuotaMap } from '../api/types'
import { exhaustionMarker, parseCreditKey, resetCreditOffers, resetCreditOfferText } from './resetCreditOffer.ts'

const NOW = Date.parse('2026-11-08T06:30:00Z')
const FUTURE = '2026-11-08T10:00:00Z'
const PAST = '2026-11-08T05:00:00Z'
const win = (used: number, resets: string | null) => ({ used_pct: used, resets_at: resets, observed_at: null }) as never
const q = (over: Partial<KindQuota> & { available?: number } = {}): KindQuota =>
  ({
    five_hour: null,
    seven_day: null,
    fable: null,
    reset_credits: { available: over.available ?? 2, title: 'Full reset', expires_at: '2026-11-30T00:00:00Z', credits: [] },
    limit_hit: null,
    plan: null,
    updated_at: '',
    source: 'codex-app-server',
    account: null,
    host: 'local',
    stale: false,
    ...over,
  }) as unknown as KindQuota

test('撞限橫幅（還沒過期）或窗口 100% 且重置還在後面＝用完；其餘不算', () => {
  assert.equal(exhaustionMarker(q({ limit_hit: { message: 'x', until: FUTURE, at: '2026-11-08T06:00:00Z', bucket: null } as never }), NOW), 'hit:2026-11-08T06:00:00Z')
  assert.equal(exhaustionMarker(q({ limit_hit: { message: 'x', until: null, at: 'A', bucket: null } as never }), NOW), 'hit:A', '沒寫到期＝等下一回合，仍算用完')
  assert.equal(exhaustionMarker(q({ limit_hit: { message: 'x', until: PAST, at: 'A', bucket: null } as never }), NOW), null, '橫幅已過期')
  assert.equal(exhaustionMarker(q({ seven_day: win(100, FUTURE) }), NOW), `win:seven_day:${FUTURE}`)
  assert.equal(exhaustionMarker(q({ five_hour: win(100, PAST) }), NOW), null, '窗口已重置')
  assert.equal(exhaustionMarker(q({ five_hour: win(80, FUTURE) }), NOW), null, '還有剩')
})

test('有券才問；沒券、stale、非 codex 都不問', () => {
  const quota: QuotaMap = {
    codex: q({ available: 0, limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never }),
    'm4p/codex:co0': q({ stale: true, limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never }),
    claude: q({ limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never }),
  }
  assert.deepEqual(resetCreditOffers({}, quota, NOW).offers, [])
})

test('同一次耗盡只問一次（不論用或不用都記下）；新的耗盡事件才再問', () => {
  const hit = q({ limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never })
  const first = resetCreditOffers({}, { 'm4p/codex:co0': hit }, NOW)
  assert.equal(first.offers.length, 1)
  assert.deepEqual(first.offers[0], {
    id: 'm4p/codex:co0|hit:A',
    key: 'm4p/codex:co0',
    host: 'm4p',
    identity: 'co0',
    available: 2,
    title: 'Full reset',
    expiresAt: '2026-11-30T00:00:00Z',
    heldBots: [],
  })
  const again = resetCreditOffers(first.next, { 'm4p/codex:co0': hit }, NOW + 60_000)
  assert.deepEqual(again.offers, [], '同一次耗盡不重問')
  const newHit = q({ limit_hit: { message: 'x', until: FUTURE, at: 'B', bucket: null } as never })
  assert.equal(resetCreditOffers(again.next, { 'm4p/codex:co0': newHit }, NOW).offers.length, 1, '新的撞限紀錄＝新事件')
})

test('提示列出因額度停工的 bot 名稱（daemon 的 held_bots）', () => {
  const hit = q({ limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never, held_bots: ['alpha', 'beta'] })
  const [offer] = resetCreditOffers({}, { 'm4p/codex:co0': hit }, NOW).offers
  assert.deepEqual(offer.heldBots, ['alpha', 'beta'])
  assert.match(resetCreditOfferText(offer), /停工的 bot：alpha、beta。/)
  const none = q({ limit_hit: { message: 'x', until: FUTURE, at: 'A', bucket: null } as never })
  const [bare] = resetCreditOffers({}, { 'm4p/codex:co0': none }, NOW).offers
  assert.deepEqual(bare.heldBots, [])
  assert.doesNotMatch(resetCreditOfferText(bare), /停工的 bot/)
})

test('key 拆成主機與身份', () => {
  assert.deepEqual(parseCreditKey('codex'), { host: 'local', identity: null })
  assert.deepEqual(parseCreditKey('codex:co0'), { host: 'local', identity: 'co0' })
  assert.deepEqual(parseCreditKey('m4p/codex'), { host: 'm4p', identity: null })
  assert.deepEqual(parseCreditKey('m4p/codex:co0'), { host: 'm4p', identity: 'co0' })
})
