/**
 * 分享頁字典對帳（issue #1932）：字典 key 與分享頁原始碼的 `t('…')` 互相對得上，插值名稱中英一致。
 * 主 UI 的對帳在 `lib/i18n/i18nDict.test.ts`；分享頁字典不在 EN_PARTS 裡，所以在這裡另做。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { SHARE_EN } from './shareI18n.ts'

const dir = new URL('.', import.meta.url).pathname
const sources = readdirSync(dir)
  .filter((f) => /\.tsx?$/.test(f) && !f.includes('.test.') && f !== 'shareI18n.ts')
  .map((f) => readFileSync(join(dir, f), 'utf8'))
  .join('\n')
const used = new Set([...sources.matchAll(/\bt\(\s*'((?:[^'\\]|\\.)*)'/g)].map((m) => m[1]))
const placeholders = (s: string) => [...s.matchAll(/\{([A-Za-z_]\w*)\}/g)].map((m) => m[1]).sort().join(',')

test('分享頁每個 t() 的 key 都在字典裡', () => {
  const missing = [...used].filter((k) => !(k in SHARE_EN))
  assert.deepEqual(missing, [])
})

test('分享頁字典的每個 key 都真的在原始碼裡用到', () => {
  const unused = Object.keys(SHARE_EN).filter((k) => !used.has(k))
  assert.deepEqual(unused, [])
})

test('分享頁字典的插值名稱中英一致', () => {
  for (const [k, v] of Object.entries(SHARE_EN)) {
    const en = typeof v === 'string' ? v : v.other
    assert.equal(placeholders(en), placeholders(k), k)
  }
})
