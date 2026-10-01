import test from 'node:test'
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

const source = await readFile(new URL('./DirPicker.tsx', import.meta.url), 'utf8')
const handler = source.match(/const onKeyDown = \(e: ReactKeyboardEvent\) => \{([\s\S]*?)\n  \}\n/)?.[1]

// 整個對話框掛一個 onKeyDown：焦點在「取消」「上一層」勾選格上按 Enter／方向鍵，不能被它攔去選目錄或換層。
test('DirPicker 的容器快捷鍵先放行按鈕／勾選格自己的鍵（Esc 除外）', () => {
  assert.ok(handler, 'DirPicker 要有 onKeyDown')
  const guard = "if (e.key !== 'Escape' && keyBelongsToControl(e.target)) return"
  assert.ok(handler.includes(guard), '要有 keyBelongsToControl 放行')
  assert.ok(handler.indexOf(guard) < handler.indexOf("e.key === 'Enter'"), '放行要在 Enter 處理之前')
  assert.ok(handler.indexOf(guard) < handler.indexOf("e.key === 'ArrowDown'"), '放行要在方向鍵處理之前')
})
