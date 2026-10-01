import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const source = await readFile(new URL('./Sidebar.tsx', import.meta.url), 'utf8')

// project 標題整列 onClick＝選取專案（開群組對話）。⋯ 選單的觸發鍵與選單空白處的點擊會冒泡上去，
// 開選單就被帶去群組頁；bot 列的 `.bot-actions` 同樣用 stopPropagation 擋掉。
test('project 標題的動作區（＋、⋯）擋掉冒泡，點它們不會選取專案', () => {
  assert.match(source, /<span\s+className="project-head-actions"\s+onClick=\{\(e\) => e\.stopPropagation\(\)\}>/)
})
