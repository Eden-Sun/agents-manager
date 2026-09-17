import test from 'node:test'
import assert from 'node:assert/strict'
import { surveyDraftAllowed, writeShared } from './mobilePreview.ts'
import { saveCounts } from './unread.ts'

test('手機預覽裡不寫共用的 localStorage：預覽的整份 map 不能蓋掉主畫面的（review3 c1 L8）', () => {
  const writes: string[] = []
  writeShared(() => writes.push('preview'), true)
  writeShared(() => writes.push('main'), false)
  assert.deepEqual(writes, ['main'])
})

test('主畫面照常寫（預設就是不在預覽裡）', () => {
  const map = new Map<string, string>()
  ;(globalThis as unknown as { localStorage: unknown }).localStorage = {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
  }
  saveCounts({ bots: { b1: 2 }, groups: {} })
  assert.equal(map.size, 1)
})

test('手機預覽裡的多分頁問卷不自動預載（預載會自己送導覽鍵，跟主畫面互相插隊）', () => {
  assert.equal(surveyDraftAllowed(true, true), false)
  assert.equal(surveyDraftAllowed(true, false), true)
  assert.equal(surveyDraftAllowed(false, false), false)
})
