import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const source = await readFile(new URL('./Sidebar.tsx', import.meta.url), 'utf8')

test('project 標題的動作區（＋、⋯）擋掉冒泡，點它們不會選取專案', () => {
  assert.match(source, /<span\s+className="project-head-actions"\s+onClick=\{\(e\) => e\.stopPropagation\(\)\}>/)
})
