/**
 * 英文字典與原始碼的對帳（issue #1932）。並行抽字串時各區塊各改各的檔，這支擋住的是：
 * ① 字典有 key 但原始碼沒有（改了原句忘了改字典、或抽錯 key）② `t()` 的 key 不是字面字串、或插值名稱和呼叫對不上
 * ③ 英文的 `{name}` 跟 key 不一致 ④ 兩個區塊對同一句寫了不同英文 ⑤ 複數少一邊。
 * 缺英文**不是**錯（退回繁中）；列出缺了哪些用 `bun scripts/i18n-report.ts --missing`。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { EN_CONFLICTS, EN_PARTS } from '../../i18n/en'
import { AREA_IDS, AREA_FILES } from './areas'
import { isUiSource, placeholderNames, scanKeys } from './i18nScan'
import { entryStrings } from './translate'

const root = fileURLToPath(new URL('../..', import.meta.url)).replace(/\/$/, '')
function walk(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) walk(p, out)
    else out.push(p)
  }
  return out
}
const sources = walk(root)
  .map((p) => 'src/' + relative(root, p))
  .filter(isUiSource)
const scanned = sources.map((f) => {
  const text = readFileSync(join(root, '..', f), 'utf8')
  // 沒提到 i18n 的檔案不用 parse（整個 src 約七百個檔，parse 全部要好幾秒）。
  return text.includes('i18n') ? scanKeys(f, text) : { uses: [], problems: [] }
})
const uses = scanned.flatMap((s) => s.uses)
const usedKeys = new Set(uses.map((u) => u.key))

test('t()／msg() 的 key 都是字面字串，插值名稱和呼叫對得上', () => {
  const problems = scanned.flatMap((s) => s.problems).map((p) => `${p.file}:${p.line} ${p.message}`)
  assert.deepEqual(problems, [])
})

test('字典的每個 key 都真的出現在原始碼（t／msg 的第一個參數）', () => {
  const unused = Object.entries(EN_PARTS).flatMap(([area, dict]) => Object.keys(dict).filter((k) => !usedKeys.has(k)).map((k) => `${area}: ${k}`))
  assert.deepEqual(unused, [])
})

test('英文的插值名稱和 key 一致，複數兩邊都有', () => {
  const bad: string[] = []
  for (const [area, dict] of Object.entries(EN_PARTS)) {
    for (const [key, entry] of Object.entries(dict)) {
      const want = placeholderNames(key).sort().join(',')
      if (typeof entry !== 'string' && (typeof entry.one !== 'string' || typeof entry.other !== 'string')) bad.push(`${area}: ${key} 複數少了 one／other`)
      if (typeof entry !== 'string' && !placeholderNames(key).includes('n')) bad.push(`${area}: ${key} 複數的 key 要含 {n}`)
      for (const s of entryStrings(entry)) {
        const got = placeholderNames(s).sort().join(',')
        if (got !== want) bad.push(`${area}: ${key} → ${s}（插值 {${got}} ≠ {${want}}）`)
      }
    }
  }
  assert.deepEqual(bad, [])
})

test('英文不含全形標點（括號、冒號、逗號、句號、斜線都用半形）', () => {
  const fullwidth = /[\u3001-\u3003\u3008-\u3011\u3014-\u301f\uff01-\uff5e]/u
  const bad = Object.entries(EN_PARTS).flatMap(([area, dict]) =>
    Object.entries(dict).flatMap(([key, entry]) => entryStrings(entry).filter((s) => fullwidth.test(s)).map((s) => `${area}: ${key} → ${s}`)),
  )
  assert.deepEqual(bad, [])
})

test('不同區塊對同一句寫了不同的英文（相同沒關係）', () => {
  assert.deepEqual(EN_CONFLICTS, [])
})

test('分區表：列出的檔案都存在、沒有重複歸屬', () => {
  const all = AREA_IDS.flatMap((id) => AREA_FILES[id])
  assert.deepEqual(all.filter((f, i) => all.indexOf(f) !== i), [])
  assert.deepEqual(all.filter((f) => !sources.includes('src/' + f)), [], '檔案被刪或改名了：更新 areas.ts')
})
