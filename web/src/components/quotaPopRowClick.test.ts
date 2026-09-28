import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const source = await readFile(new URL('./QuotaStrip.tsx', import.meta.url), 'utf8')
const popRow = source.match(/function PopRow\([\s\S]*?(?=\n\/\*\*)/)?.[0]

test('portal 確認框裡的點擊不會切換額度列', () => {
  assert.ok(popRow, 'QuotaStrip.tsx 要有 PopRow')
  const clickHandler = popRow.match(/onClick=\{\(e\) => \{([\s\S]*?)\n\s*\}\}/)?.[1]
  assert.ok(clickHandler, 'PopRow 要有列點擊處理')
  const portalGuard = 'if (!e.currentTarget.contains(e.target as Node)) return'
  assert.ok(clickHandler.includes(portalGuard), '離開列 DOM 的 portal 點擊要被忽略')
  assert.ok(clickHandler.indexOf(portalGuard) < clickHandler.indexOf('toggle()'), 'portal guard 必須在切換前執行')
})
