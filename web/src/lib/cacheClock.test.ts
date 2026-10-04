import test from 'node:test'
import assert from 'node:assert/strict'
import { cacheLevel, cacheState } from './cacheClock.ts'

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
  assert.match(s.title, /^快取約 45 分後到期（上次活動 \d\d:\d\d）$/)
  // 15 秒後重算：少 15 秒。
  assert.equal(cacheState(at(15), 3600, T0 + 15_000)!.remainingSecs, 45 * 60 - 15)
  assert.equal(cacheState(at(50), 3600, T0)!.level, 'warn')
  assert.equal(cacheState(at(57), 3600, T0)!.level, 'low')
  // 不到一分鐘也說「約 1 分」，不說 0 分。
  assert.match(cacheState(at(59.9), 3600, T0)!.title, /約 1 分後到期/)
})

test('到期：已涼、比例 0', () => {
  const s = cacheState(at(61), 3600, T0)!
  assert.equal(s.level, 'cold')
  assert.equal(s.frac, 0)
  assert.match(s.title, /^快取已涼（上次活動 \d\d:\d\d）$/)
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
