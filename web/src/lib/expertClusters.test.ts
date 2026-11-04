import test from 'node:test'
import assert from 'node:assert/strict'
import type { Lamp } from '../api/types.ts'
import { clusterStatCounts, clusterStatOf, clusterStatText } from './expertClusters.ts'

const ALL_LAMPS: Lamp[] = ['working', 'idle', 'blocked', 'starting', 'stopping', 'offline', 'disconnected', 'unknown']

/** 把文字拆回 `N 標籤` 的段落，加總 N 應該等於成員數。 */
const sumOf = (text: string) => (text === '' ? 0 : text.split(' · ').reduce((acc, part) => acc + Number(part.split(' ')[0]), 0))

test('clusterStatText：例子「1 執行中 · 1 閒置 · 2 停止」，0 的項目省略', () => {
  assert.equal(clusterStatText(['working', 'idle', 'offline', 'offline']), '1 執行中 · 1 閒置 · 2 停止')
  assert.equal(clusterStatText(['idle', 'idle', 'idle']), '3 閒置')
  assert.equal(clusterStatText([]), '')
})

test('clusterStatText：各狀態計數加總＝成員數，0 的項目不出現', () => {
  const samples: Lamp[][] = [
    ['working'],
    ['idle', 'offline'],
    ['starting', 'starting', 'stopping'],
    ['blocked', 'disconnected', 'unknown', 'working'],
    ALL_LAMPS,
    [...ALL_LAMPS, ...ALL_LAMPS, 'idle'],
  ]
  for (const lamps of samples) {
    const text = clusterStatText(lamps)
    assert.equal(sumOf(text), lamps.length, text)
    for (const part of text.split(' · ')) assert.doesNotMatch(part, /^0 /)
  }
})

test('clusterStatText：offline 與 stopping 歸「停止」，啟動中、等待回應、連線中斷、狀態未知各自列出', () => {
  assert.equal(clusterStatOf('offline'), 'stopped')
  assert.equal(clusterStatOf('stopping'), 'stopped')
  assert.equal(clusterStatText(['stopping', 'offline']), '2 停止')
  assert.equal(clusterStatText(['starting', 'blocked', 'disconnected', 'unknown']), '1 等待回應 · 1 啟動中 · 1 連線中斷 · 1 狀態未知')
  assert.equal(clusterStatText(['starting']), '1 啟動中')
})

test('clusterStatCounts：照固定順序、0 的不列、加總＝成員數', () => {
  assert.deepEqual(clusterStatCounts(['idle', 'working', 'offline', 'idle']), [
    { stat: 'working', n: 1 },
    { stat: 'idle', n: 2 },
    { stat: 'stopped', n: 1 },
  ])
  assert.deepEqual(clusterStatCounts([]), [])
})
