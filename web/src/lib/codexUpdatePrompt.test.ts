import test from 'node:test'
import assert from 'node:assert/strict'
import { answerCodexUpdate, CODEX_UPDATE_MOVED_ON, parseCodexUpdatePrompt } from './codexUpdatePrompt.ts'

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

test('其他畫面一律不認，寧可不顯示也不要拿錯版本去查', () => {
  assert.equal(parseCodexUpdatePrompt('Do you want to allow this command? (y/n)'), null)
  assert.equal(parseCodexUpdatePrompt(null), null)
})

// ── 送出那一下（issue #546）：按鈕依據的是每秒輪詢的快照，送之前要重讀 ──

const SCREEN = (from: string, to: string) => `  ✨ Update available! ${from} -> ${to}\n  1. Update now\n  2. Not now\n`
/** codex 的核准框：同樣是 `1.` 開頭，那顆 1 落進來就等於核准了別的東西。 */
const APPROVAL = '  Allow command `rm -rf build`?\n  1. Yes\n  2. No\n'

function io(screen: string | null) {
  const pressed: string[][] = []
  return { pressed, read: async () => screen, press: (keys: string[]) => pressed.push(keys) }
}

test('#546：還停在同一個升級提示才送出', async () => {
  const want = parseCodexUpdatePrompt(SCREEN('0.154.0', '0.155.1'))!
  const live = io(SCREEN('0.154.0', '0.155.1'))
  assert.equal(await answerCodexUpdate(live, want, '1'), null)
  assert.deepEqual(live.pressed, [['1']])
})

test('#546：畫面換掉了就不送，講一句讓人再點一次', async () => {
  const want = parseCodexUpdatePrompt(SCREEN('0.154.0', '0.155.1'))!
  for (const screen of [APPROVAL, null, '', SCREEN('0.155.1', '0.156.0'), SCREEN('0.153.4', '0.155.1')]) {
    const gone = io(screen)
    assert.equal(await answerCodexUpdate(gone, want, '1'), CODEX_UPDATE_MOVED_ON, JSON.stringify(screen))
    assert.deepEqual(gone.pressed, [], `不該送出任何鍵：${JSON.stringify(screen)}`)
  }
})

