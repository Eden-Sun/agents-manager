import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import { AREA_IDS, areaOf, type AreaId } from '../src/lib/i18n/areas'
import { findHardcodedCjk, isUiSource, scanKeys, staleIgnores, type IgnoreEntry } from '../src/lib/i18n/i18nScan'
import ignoreFile from '../src/i18n/ignore.json' with { type: 'json' }

const IGNORE: IgnoreEntry[] = (ignoreFile as { entries: IgnoreEntry[] }).entries
const sourceOf = (file: string): string | undefined => (existsSync(file) ? readFileSync(file, 'utf8') : undefined)

function walk(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) walk(p, out)
    else out.push(p)
  }
  return out
}

const args = process.argv.slice(2)
const files = walk('src').filter(isUiSource)
if (args[0] === '--missing') {
  const { EN } = await import('../src/i18n/en')
  const { SHARE_EN } = await import('../src/share/shareI18n')
  const seen = new Set<string>()
  for (const f of files) {
    const dict = f.startsWith('src/share/') ? SHARE_EN : EN
    for (const u of scanKeys(f, readFileSync(f, 'utf8')).uses) {
      if (!Object.hasOwn(dict, u.key) && !seen.has(u.key)) {
        seen.add(u.key)
        console.log(`${f}:${u.line}  ${u.key}`)
      }
    }
  }
  console.log(`\n${seen.size} 個 key 還沒有英文`)
} else if (args[0] === '--lines') {
  const f = args[1]
  for (const h of findHardcodedCjk(f, readFileSync(f, 'utf8'), IGNORE)) console.log(`${f}:${h.line}  ${h.text}`)
} else {
  const area = args[0] === '--area' ? (args[1] as AreaId) : null
  if (args[0] === '--area' && !AREA_IDS.includes(area as AreaId)) throw new Error(`區塊要是 ${AREA_IDS.join('／')}`)
  const prefix = area ? '' : (args[0] ?? '')
  const rows = files
    .filter((f) => f.startsWith(prefix) && (!area || areaOf(f) === area))
    .map((f) => ({ f, n: findHardcodedCjk(f, readFileSync(f, 'utf8'), IGNORE).length }))
    .filter((r) => r.n > 0)
    .sort((a, b) => b.n - a.n)
  for (const r of rows) console.log(`${String(r.n).padStart(4)}  ${r.f}`)
  console.log(`\n共 ${rows.reduce((s, r) => s + r.n, 0)} 處寫死中文，分布在 ${rows.length} 個檔案（已排除 ${IGNORE.length} 條，src/i18n/ignore.json）`)
  const stale = staleIgnores(IGNORE, sourceOf)
  if (stale.length) {
    console.log(`\n過期的排除條目（對不上任何寫死中文，請從 src/i18n/ignore.json 刪除）：`)
    for (const e of stale) console.log(`  ${e.file}  ${e.text}`)
  }
}
