// 多語系進度報告（issue #1932）。在 web/ 底下跑：
//   bun scripts/i18n-report.ts                  # 每個檔案還沒抽出去的寫死中文數（照數量排序）
//   bun scripts/i18n-report.ts src/components   # 只看這個路徑前綴
//   bun scripts/i18n-report.ts --area chat      # 只看某個分區（chat／bots／hosts／updates／core，見 src/lib/i18n/areas.ts）
//   bun scripts/i18n-report.ts --lines <檔案>   # 列出該檔每一處寫死中文
//   bun scripts/i18n-report.ts --missing        # t()／msg() 的 key 裡還沒有英文的
// 規則見 docs/FRONTEND.md〈多語系〉。
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import { AREA_IDS, areaOf, type AreaId } from '../src/lib/i18n/areas'
import { findHardcodedCjk, isUiSource, scanKeys } from '../src/lib/i18n/i18nScan'

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
  const seen = new Set<string>()
  for (const f of files) {
    for (const u of scanKeys(f, readFileSync(f, 'utf8')).uses) {
      if (!Object.hasOwn(EN, u.key) && !seen.has(u.key)) {
        seen.add(u.key)
        console.log(`${f}:${u.line}  ${u.key}`)
      }
    }
  }
  console.log(`\n${seen.size} 個 key 還沒有英文`)
} else if (args[0] === '--lines') {
  const f = args[1]
  for (const h of findHardcodedCjk(f, readFileSync(f, 'utf8'))) console.log(`${f}:${h.line}  ${h.text}`)
} else {
  const area = args[0] === '--area' ? (args[1] as AreaId) : null
  if (args[0] === '--area' && !AREA_IDS.includes(area as AreaId)) throw new Error(`區塊要是 ${AREA_IDS.join('／')}`)
  const prefix = area ? '' : (args[0] ?? '')
  const rows = files
    .filter((f) => f.startsWith(prefix) && (!area || areaOf(f) === area))
    .map((f) => ({ f, n: findHardcodedCjk(f, readFileSync(f, 'utf8')).length }))
    .filter((r) => r.n > 0)
    .sort((a, b) => b.n - a.n)
  for (const r of rows) console.log(`${String(r.n).padStart(4)}  ${r.f}`)
  console.log(`\n共 ${rows.reduce((s, r) => s + r.n, 0)} 處寫死中文，分布在 ${rows.length} 個檔案`)
}
