import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { answerCodexUpdate, CODEX_UPDATE_MOVED_ON, isCodexUpdateMenu, parseCodexUpdatePrompt } from './codexUpdatePrompt.ts'
import { parseChoiceMenu } from './tuiChoices.ts'

const ANSI = new RegExp(`${String.fromCharCode(27)}\\[[0-?]*[ -/]*[@-~]`, 'g')
const RAW_DRAFT_BOX = readFileSync(new URL('../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi', import.meta.url), 'utf8')
const DRAFT_BOX = RAW_DRAFT_BOX.replace(ANSI, '')
const RATE_LIMIT_MENU = readFileSync(new URL('../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt', import.meta.url), 'utf8')

test('讀得出 codex TUI 那句的前後版本', () => {
  const screen = [
    '  ✨ Update available! 0.153.4 -> 0.154.0',
    '  Release notes: https://github.com/openai/codex/releases',
    "> 1. Update now (runs `sh -c 'curl -fsSL ...'`)",
    '  2. Skip',
  ].join('\n')
  assert.deepEqual(parseCodexUpdatePrompt(screen), { from: '0.153.4', to: '0.154.0', interactive: true })
})

test('只有新版號的提示只提供版本資訊，不是互動選單', () => {
  const screen = 'Update available! v0.154.0'
  assert.deepEqual(parseCodexUpdatePrompt(screen), { from: null, to: '0.154.0', interactive: false })
  assert.equal(isCodexUpdateMenu(screen), false)
})

test('開場 ANSI 安裝方框只提供版本資訊，不是互動選單', () => {
  assert.deepEqual(parseCodexUpdatePrompt(RAW_DRAFT_BOX), { from: '0.155.1', to: '0.156.1', interactive: false })
})

test('開場安裝方框只提供版本資訊，不蓋掉後面的真選單', () => {
  const screen = `${DRAFT_BOX}\n${RATE_LIMIT_MENU}`
  assert.deepEqual(parseCodexUpdatePrompt(screen), { from: '0.155.1', to: '0.156.1', interactive: false })
  assert.equal(isCodexUpdateMenu(screen), false)
  const choices = parseChoiceMenu(screen)?.choices
  assert.deepEqual(choices?.map((choice) => choice.number), [1, 2, 3])
  assert.match(choices?.[0].title ?? '', /^Switch to gpt-6-luna\b/)
  assert.match(choices?.[2].title ?? '', /^Keep current model \(never show again\)/)
})

test('其他畫面一律不認，寧可不顯示也不要拿錯版本去查', () => {
  assert.equal(parseCodexUpdatePrompt('Do you want to allow this command? (y/n)'), null)
  assert.equal(parseCodexUpdatePrompt(null), null)
})

// ── 送出那一下（issue #546）：按鈕依據的是每秒輪詢的快照，送之前要重讀 ──

const SCREEN = (from: string, to: string) => `  ✨ Update available! ${from} -> ${to}\n> 1. Update now\n  2. Skip\n  3. Skip until next version\n`
/** codex 的核准框：同樣是 `1.` 開頭，那顆 1 落進來就等於核准了別的東西。 */
const APPROVAL = '  Allow command `rm -rf build`?\n  1. Yes\n  2. No\n'

function io(screen: string | null) {
  const pressed: string[][] = []
  return { pressed, read: async () => screen, press: (keys: string[]) => pressed.push(keys) }
}

test('#546：還停在同一個升級提示才送出', async () => {
  const screen = SCREEN('0.154.0', '0.155.1')
  const want = parseCodexUpdatePrompt(screen)!
  const live = io(screen)
  assert.equal(isCodexUpdateMenu(screen), true)
  assert.equal(isCodexUpdateMenu(screen, '3'), true)
  assert.equal(await answerCodexUpdate(live, want, '1'), null)
  assert.deepEqual(live.pressed, [['1']])
})

test('開場方框仍在、但目前停在別的選單時不可送數字', async () => {
  const screen = `${DRAFT_BOX}\n${RATE_LIMIT_MENU}`
  const want = parseCodexUpdatePrompt(screen)!
  const live = io(screen)
  assert.equal(await answerCodexUpdate(live, want, '3'), CODEX_UPDATE_MOVED_ON)
  assert.deepEqual(live.pressed, [])
})

test('#546：畫面換掉了就不送，講一句讓人再點一次', async () => {
  const want = parseCodexUpdatePrompt(SCREEN('0.154.0', '0.155.1'))!
  for (const screen of [APPROVAL, null, '', SCREEN('0.155.1', '0.156.0'), SCREEN('0.153.4', '0.155.1')]) {
    const gone = io(screen)
    assert.equal(await answerCodexUpdate(gone, want, '1'), CODEX_UPDATE_MOVED_ON, JSON.stringify(screen))
    assert.deepEqual(gone.pressed, [], `不該送出任何鍵：${JSON.stringify(screen)}`)
  }
})
