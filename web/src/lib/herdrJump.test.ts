import test from 'node:test'
import assert from 'node:assert/strict'
import { herdrJumpCommand } from './herdrJump.ts'

test('先 focus 那顆 pane 再開 TUI', () => {
  assert.equal(
    herdrJumpCommand('agents-manager', 'w168:p5H'),
    'herdr --session agents-manager agent focus w168:p5H >/dev/null && herdr --session agents-manager',
  )
})

test('沒有 session 用預設；沒有 pane 就不給；奇怪字元加引號', () => {
  assert.equal(herdrJumpCommand(null, 'w1:p1'), 'herdr --session agents-manager agent focus w1:p1 >/dev/null && herdr --session agents-manager')
  assert.equal(herdrJumpCommand('agents-manager', ''), '')
  assert.equal(herdrJumpCommand("my sess", 'w1:p1'), "herdr --session 'my sess' agent focus w1:p1 >/dev/null && herdr --session 'my sess'")
})
