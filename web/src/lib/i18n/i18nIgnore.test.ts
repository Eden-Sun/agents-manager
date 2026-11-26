import test from 'node:test'
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { staleIgnores, type IgnoreEntry } from './i18nScan'
import ignoreFile from '../../i18n/ignore.json' with { type: 'json' }

const webRoot = fileURLToPath(new URL('../../..', import.meta.url)).replace(/\/$/, '')
const entries = (ignoreFile as { entries: IgnoreEntry[] }).entries
const source = (file: string): string | undefined => {
  const p = join(webRoot, file)
  return existsSync(p) ? readFileSync(p, 'utf8') : undefined
}

test('ignore.json 結構正確：有 entries 陣列，每筆有 file 與 text，不重複', () => {
  assert.ok(Array.isArray(entries))
  for (const e of entries) {
    assert.equal(typeof e.file, 'string')
    assert.equal(typeof e.text, 'string')
    assert.ok(e.text.length > 0, `空字串條目：${e.file}`)
    assert.ok(e.file.startsWith('src/'), `路徑要從 src/ 起：${e.file}`)
  }
  const keys = entries.map((e) => `${e.file}\u0000${e.text}`)
  assert.equal(new Set(keys).size, keys.length, '有重複條目')
})

test('ignore.json 沒有過期條目：檔案還在、而且檔裡還有這段寫死中文', () => {
  const stale = staleIgnores(entries, source)
  assert.deepEqual(stale, [], `過期條目（請從 src/i18n/ignore.json 刪掉，或改成新的文字）：\n${stale.map((e) => `  ${e.file}  ${e.text}`).join('\n')}`)
})
