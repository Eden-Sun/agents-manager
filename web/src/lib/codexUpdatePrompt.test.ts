import test from 'node:test'
import assert from 'node:assert/strict'
import { parseCodexUpdatePrompt } from './codexUpdatePrompt.ts'

test('讀得出 codex TUI 那句的前後版本', () => {
  const screen = [
    '  ✨ Update available! 0.153.4 -> 0.154.0',
    '  Release notes: https://github.com/openai/codex/releases',
    "> 1. Update now (runs `sh -c 'curl -fsSL ...'`)",
    '  2. Skip',
  ].join('\n')
  assert.deepEqual(parseCodexUpdatePrompt(screen), { from: '0.153.4', to: '0.154.0' })
})

test('只寫新版時也認', () => {
  assert.deepEqual(parseCodexUpdatePrompt('Update available! v0.154.0'), { from: null, to: '0.154.0' })
})

test('雙版本提示的箭頭後版本缺失或格式不完整時不拿舊版當新版', () => {
  assert.equal(parseCodexUpdatePrompt('Update available! 0.153.4 ->'), null)
  assert.equal(parseCodexUpdatePrompt('Update available! 0.153.4 -> v0.'), null)
  assert.equal(parseCodexUpdatePrompt('Update available! 0.153.4 → not-a-version'), null)
})

test('其他畫面一律不認，寧可不顯示也不要拿錯版本去查', () => {
  assert.equal(parseCodexUpdatePrompt('Do you want to allow this command? (y/n)'), null)
  assert.equal(parseCodexUpdatePrompt(null), null)
})
