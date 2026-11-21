import { useStore } from '../store/store'
import test from 'node:test'
import assert from 'node:assert/strict'
import { CACHE_RED_TICK_MS, CACHE_TICK_MS, apiChipCache, cacheLevel, cacheState, chipCache } from './cacheClock.ts'

const T0 = Date.parse('2026-10-04T12:00:00.000Z')
const at = (minAgo: number) => new Date(T0 - minAgo * 60_000).toISOString()

test('門檻：> 15 分綠、5–15 分黃、< 5 分紅、到期已涼', () => {
  assert.equal(cacheLevel(16 * 60), 'fresh')
  assert.equal(cacheLevel(15 * 60), 'warn')
  assert.equal(cacheLevel(5 * 60), 'warn')
  assert.equal(cacheLevel(5 * 60 - 1), 'low')
  assert.equal(cacheLevel(1), 'low')
  assert.equal(cacheLevel(0), 'cold')
})

test('倒數：剩餘比例＝剩餘／TTL，隨時間往下掉', () => {
  const s = cacheState(at(15), 3600, T0)!
  assert.equal(s.remainingSecs, 45 * 60)
  assert.equal(s.frac, 0.75)
  assert.equal(s.level, 'fresh')
  assert.match(s.title, /^快取約 45 分後到期（上次活動 (?:\d+\/\d+ )?\d\d:\d\d）$/)
  assert.equal(cacheState(at(15), 3600, T0 + 15_000)!.remainingSecs, 45 * 60 - 15)
  assert.equal(cacheState(at(50), 3600, T0)!.level, 'warn')
  assert.equal(cacheState(at(57), 3600, T0)!.level, 'low')
  assert.match(cacheState(at(59.9), 3600, T0)!.title, /約 1 分後到期/)
})

test('上次活動不是今天時帶月日；同一天仍只寫時分', () => {
  useStore.setState({ userTimeZone: Intl.DateTimeFormat().resolvedOptions().timeZone })
  const now = new Date(2026, 9, 11, 15, 0).getTime()
  const last = new Date(2026, 9, 8, 14, 5).toISOString()
  assert.equal(cacheState(last, 3600, now)!.title, '快取已涼（上次活動 10/8 14:05）')

  const today = new Date(2026, 9, 11, 14, 5).toISOString()
  assert.equal(cacheState(today, 3600, now)!.title, '快取約 5 分後到期（上次活動 14:05）')
})

test('到期：已涼、比例 0', () => {
  const s = cacheState(at(61), 3600, T0)!
  assert.equal(s.level, 'cold')
  assert.equal(s.frac, 0)
  assert.match(s.title, /^快取已涼（上次活動 (?:\d+\/\d+ )?\d\d:\d\d）$/)
})

test('回合進行中＝滿條；TTL 不明（grok）或沒有紀錄不畫', () => {
  assert.deepEqual(
    { ...cacheState(at(120), 3600, T0, true)!, title: '' },
    { level: 'fresh', remainingSecs: 3600, frac: 1, title: '' },
  )
  assert.equal(cacheState(at(1), null, T0), null)
  assert.equal(cacheState(at(1), undefined, T0, true), null)
  assert.equal(cacheState(null, 3600, T0), null)
  assert.equal(cacheState('壞掉', 3600, T0), null)
})

test('daemon 時鐘比這台快：剩餘不超過 TTL', () => {
  assert.equal(cacheState(new Date(T0 + 60_000).toISOString(), 3600, T0)!.frac, 1)
})

test('保溫：顏色從保溫時間起算，tooltip 仍講真實的上次活動', () => {
  const s = cacheState(at(70), 3600, T0, false, at(10))!
  assert.equal(s.remainingSecs, 50 * 60)
  assert.equal(s.level, 'fresh')
  assert.match(s.title, /^快取約 50 分後到期（上次活動 (?:\d+\/\d+ )?\d\d:\d\d，已保溫 (?:\d+\/\d+ )?\d\d:\d\d）$/)
  assert.equal(cacheState(at(130), 3600, T0, false, at(61))!.level, 'cold')
  assert.deepEqual(cacheState(at(20), 3600, T0, false, at(30)), cacheState(at(20), 3600, T0))
  assert.deepEqual(cacheState(at(20), 3600, T0, false, '壞掉'), cacheState(at(20), 3600, T0))
  assert.deepEqual(cacheState(at(20), 3600, T0, false, null), cacheState(at(20), 3600, T0))
})

test('熱壓後視為涼掉：daemon 不帶 cache_kept_warm_at（熱壓前的保溫也不算），111 分前的活動就是涼', () => {
  assert.equal(cacheState(at(111), 3600, T0, false, null)!.level, 'cold')
  assert.equal(cacheState(at(111), 3600, T0, false, at(30))!.level, 'fresh')
  assert.equal(cacheState(at(2), 3600, T0, false, null)!.level, 'fresh')
})

test('chipCache：keep_warm_skip=true 無倒數、false／未帶照常倒數、切換即時生效', () => {
  const run = { last_api_at: at(15), cache_ttl_secs: 3600, cache_kept_warm_at: null as string | null, keep_warm_skip: true }
  assert.equal(chipCache(run, T0), null)
  assert.equal(chipCache(run, T0, true), null, '回合進行中也不畫')
  assert.equal(chipCache({ ...run, keep_warm_skip: false }, T0)!.level, 'fresh')
  assert.equal(chipCache({ ...run, keep_warm_skip: undefined }, T0)!.frac, 0.75)
  assert.equal(chipCache(undefined, T0), null)
  assert.deepEqual(chipCache({ ...run, keep_warm_skip: false }, T0), cacheState(run.last_api_at, 3600, T0, false, null))
})

test('chipCache：壓縮後（cache_compacted）不畫倒數，新回合後恢復', () => {
  const run = { last_api_at: at(15), cache_ttl_secs: 3600, cache_compacted: true }
  assert.equal(chipCache(run, T0), null)
  assert.equal(chipCache({ ...run, cache_compacted: false }, T0)!.level, 'fresh', '壓縮後有新回合：daemon 帶 false，倒數恢復')
  assert.equal(chipCache({ ...run, last_api_at: at(1), cache_compacted: false }, T0)!.level, 'fresh')
  assert.equal(chipCache({ ...run, cache_compacted: undefined }, T0)!.level, 'fresh')
})

test('跨年：上次活動在去年，tooltip 的時間要帶年份', () => {
  useStore.setState({ userTimeZone: Intl.DateTimeFormat().resolvedOptions().timeZone })
  const now = new Date(2027, 0, 1, 0, 20).getTime()
  const last = new Date(2026, 11, 31, 23, 30).toISOString()
  assert.equal(cacheState(last, 3600, now)!.title, '快取約 10 分後到期（上次活動 2026/12/31 23:30）')
})

test('紅色（< 5 分）底色以 5 分為滿刻度：剛變紅是滿的，5 分內流到 0', () => {
  const left = (secs: number) => new Date(T0 - (3600 - secs) * 1000).toISOString()
  assert.equal(cacheState(left(300), 3600, T0)!.level, 'warn')
  assert.equal(cacheState(left(300), 3600, T0)!.frac, 1)
  assert.equal(cacheState(left(299), 3600, T0)!.level, 'low')
  assert.equal(cacheState(left(299), 3600, T0)!.frac, 299 / 300)
  assert.equal(cacheState(left(150), 3600, T0)!.frac, 0.5)
  assert.equal(cacheState(left(1), 3600, T0)!.level, 'low')
  assert.ok(cacheState(left(1), 3600, T0)!.frac < 0.01, '剩 1 秒幾乎是 0')
  assert.equal(cacheState(left(0), 3600, T0)!.frac, 0)
})

test('綠、黃仍是剩餘／TTL（不因紅色的改法而變）', () => {
  const left = (secs: number) => new Date(T0 - (3600 - secs) * 1000).toISOString()
  assert.equal(cacheState(left(960), 3600, T0)!.level, 'fresh')
  assert.equal(cacheState(left(960), 3600, T0)!.frac, 960 / 3600)
  assert.equal(cacheState(left(600), 3600, T0)!.level, 'warn')
  assert.equal(cacheState(left(600), 3600, T0)!.frac, 600 / 3600)
  assert.equal(cacheState(left(301), 3600, T0)!.frac, 301 / 3600)
})

test('紅色重算間隔 5 秒、其他 15 秒', () => {
  assert.equal(CACHE_RED_TICK_MS, 5_000)
  assert.equal(CACHE_TICK_MS, 15_000)
})

test('apiChipCache：回合中顯示熱（滿）、依 expires_at 與 ttl 算比例與等級、過期顯示冷', () => {
  assert.equal(apiChipCache(null, T0), null)
  assert.equal(apiChipCache(undefined, T0), null)
  assert.equal(apiChipCache({ working: false, last_request_at: null, ttl_secs: null, expires_at: null }, T0), null)

  const working = apiChipCache({ working: true, ttl_secs: 300, last_request_at: at(5) }, T0)
  assert.ok(working)
  assert.equal(working.level, 'fresh')
  assert.equal(working.frac, 1)
  assert.equal(working.remainingSecs, 300)
  assert.match(working.title, /回合進行中，快取是熱的/)

  const fresh1h = apiChipCache({ working: false, ttl_secs: 3600, expires_at: new Date(T0 + 45 * 60_000).toISOString(), last_request_at: at(15) }, T0)
  assert.ok(fresh1h)
  assert.equal(fresh1h.level, 'fresh')
  assert.equal(fresh1h.frac, 0.75)
  assert.equal(fresh1h.remainingSecs, 2700)
  assert.match(fresh1h.title, /快取約 45 分後到期/)

  const warn1h = apiChipCache({ working: false, ttl_secs: 3600, expires_at: new Date(T0 + 10 * 60_000).toISOString(), last_request_at: at(50) }, T0)
  assert.ok(warn1h)
  assert.equal(warn1h.level, 'warn')
  assert.equal(warn1h.frac, 600 / 3600)
  assert.equal(warn1h.remainingSecs, 600)
  assert.match(warn1h.title, /快取約 10 分後到期/)

  const low1h = apiChipCache({ working: false, ttl_secs: 3600, expires_at: new Date(T0 + 3 * 60_000).toISOString(), last_request_at: at(57) }, T0)
  assert.ok(low1h)
  assert.equal(low1h.level, 'low')
  assert.equal(low1h.frac, 180 / 300)
  assert.equal(low1h.remainingSecs, 180)
  assert.match(low1h.title, /快取約 3 分後到期/)

  const low5m = apiChipCache({ working: false, ttl_secs: 300, expires_at: new Date(T0 + 2 * 60_000).toISOString(), last_request_at: at(3) }, T0)
  assert.ok(low5m)
  assert.equal(low5m.level, 'low')
  assert.equal(low5m.frac, 120 / 300)
  assert.equal(low5m.remainingSecs, 120)
  assert.match(low5m.title, /快取約 2 分後到期/)

  const cold = apiChipCache({ working: false, ttl_secs: 300, expires_at: new Date(T0 - 10_000).toISOString(), last_request_at: at(6) }, T0)
  assert.ok(cold)
  assert.equal(cold.level, 'cold')
  assert.equal(cold.frac, 0)
  assert.equal(cold.remainingSecs, 0)
  assert.match(cold.title, /快取已涼/)
})

test('agy 12 分 TTL：閒置 <5 分 fresh、5–10 分 warn、10–12 分 low、>12 分 cold', () => {
  assert.equal(cacheLevel(421, 720), 'fresh')
  assert.equal(cacheLevel(420, 720), 'warn')
  assert.equal(cacheLevel(120, 720), 'warn')
  assert.equal(cacheLevel(119, 720), 'low')
  assert.equal(cacheLevel(1, 720), 'low')
  assert.equal(cacheLevel(0, 720), 'cold')
  const now = Date.parse('2026-11-20T10:00:00.000Z')
  const idle = (mins: number) => new Date(now - mins * 60_000).toISOString()
  assert.equal(cacheState(idle(4), 720, now)?.level, 'fresh')
  assert.equal(cacheState(idle(7), 720, now)?.level, 'warn')
  assert.equal(cacheState(idle(11), 720, now)?.level, 'low')
  assert.equal(cacheState(idle(13), 720, now)?.level, 'cold')
  const warn = cacheState(idle(7), 720, now)
  assert.match(warn?.title ?? '', /Gemini/)
})

test('claude 門檻不變：剩 15 分 warn、剩 5 分 warn、<5 分 low', () => {
  assert.equal(cacheLevel(901, 3600), 'fresh')
  assert.equal(cacheLevel(900, 3600), 'warn')
  assert.equal(cacheLevel(300, 3600), 'warn')
  assert.equal(cacheLevel(299, 3600), 'low')
  assert.equal(cacheLevel(900), 'warn', '沒給 TTL 走預設門檻')
})
