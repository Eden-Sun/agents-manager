import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { EMPTY_INPUT, asyncPanelText, inputEquals, panelInput, panelShows, parseAsyncScreen } from './codexAsyncQuestion.ts'
import { keysToSelect, parseChoiceMenu, typedAnswerHere } from './tuiChoices.ts'
import { KEYPAD, herdrKeyFromEvent } from '../hooks/usePaneKeys.ts'

// 2026-10-21 真機（codex 0.163.0，隔離 tmux session；路徑與模型列已簡化）：issue #1879。
const fx = (name: string) => readFileSync(new URL(`./__fixtures__/${name}`, import.meta.url), 'utf8')
const COLLAPSED = fx('codex-0.163-async-collapsed.txt')
const EXPANDED = fx('codex-0.163-async-expanded.txt')
const OF_TWO = fx('codex-0.163-async-expanded-of2.txt')
const FREE_TEXT = fx('codex-0.163-async-free-text.txt')
const OTHER_TYPED = fx('codex-0.163-async-other-typed.txt')

// Rust's codex_async_question test consumes this exact manifest and every listed file.
const screenCases = JSON.parse(fx('codex-async-screen-cases.json')) as {
  file: string
  expected: { expanded: boolean; count: number } | null
}[]
for (const { file, expected } of screenCases) {
  test(`Rust/TS async-screen parity: ${file}`, () => {
    assert.deepEqual(parseAsyncScreen(fx(file)), expected)
  })
}

test('macOS hint glyphs stay labels while outgoing keys use herdr names', () => {
  assert.deepEqual(KEYPAD.filter((key) => key.label === '⇧←' || key.label === '⇧→').map((key) => key.keys), [
    ['shift+left'], ['shift+right'],
  ])
  for (const [key, expected] of [['ArrowLeft', 'shift+left'], ['ArrowRight', 'shift+right']]) {
    assert.equal(herdrKeyFromEvent({ key, shiftKey: true, metaKey: false, ctrlKey: false, altKey: false, isComposing: false } as KeyboardEvent), expected)
  }
})

test('收合、展開、多題與自由輸入的畫面各自認得出狀態與題數', () => {
  assert.deepEqual(parseAsyncScreen(COLLAPSED), { expanded: false, count: 1 })
  assert.deepEqual(parseAsyncScreen(EXPANDED), { expanded: true, count: 1 })
  assert.deepEqual(parseAsyncScreen(OF_TWO), { expanded: true, count: 2 })
  assert.deepEqual(parseAsyncScreen(FREE_TEXT), { expanded: true, count: 1 })
  assert.deepEqual(parseAsyncScreen(COLLAPSED.replace('? 1 question · 7s', '? 3 questions · 12s')), { expanded: false, count: 3 })
})

test('對話紀錄裡引用的字、只有問號的行、沒有 shift+← 的行都不算畫面上有提問', () => {
  assert.equal(parseAsyncScreen('• 畫面寫 `? 1 question` 和 shift+← to answer\n› Ask Codex to do anything\n'), null)
  assert.equal(parseAsyncScreen('  ? 1 question · 7s\n  something else\n'), null)
  assert.equal(parseAsyncScreen('  ? 0 questions\n  shift+← to answer\n'), null)
  assert.equal(parseAsyncScreen('  ? for shortcuts\n'), null)
  assert.equal(parseAsyncScreen(''), null)
  assert.equal(parseAsyncScreen(null), null)
})

test('展開後的選單照編號選單認：腳註不再被當成 Other 的說明，Other 是打字列', () => {
  const menu = parseChoiceMenu(EXPANDED)
  assert.ok(menu)
  assert.equal(menu.question, 'Which color?')
  assert.deepEqual(menu.choices.map((c) => c.title), ['red', 'blue', 'green', 'Other'])
  assert.equal(menu.choices[3].detail, '', '腳註 `enter submit ^] skip` 不是選項說明')
  assert.equal(menu.cursor, 0)
  assert.equal(menu.asyncQueue, true)
  assert.deepEqual(keysToSelect(menu, 1), ['down', 'enter'])
  assert.equal(typedAnswerHere(menu, menu.choices[3]), true)
  assert.equal(typedAnswerHere(menu, menu.choices[0]), false)
})

test('macOS panels share asyncQueue footer detection and keep Other as a typing row', () => {
  for (const name of ['codex-0.163.0-macos-expanded.txt', 'codex-0.163.0-macos-expanded-of2-synthetic.txt']) {
    const menu = parseChoiceMenu(fx(name))
    assert.ok(menu, name)
    assert.equal(menu.asyncQueue, true, name)
    assert.equal(menu.question, '測試題二：要選哪個水果？')
    assert.deepEqual(menu.choices.map((c) => c.title), ['蘋果', '香蕉', 'Other'])
    assert.equal(menu.choices[2].detail, '', 'macOS footer must not become Other detail')
    assert.equal(typedAnswerHere(menu, menu.choices[2]), true)
  }
})

for (const name of ['codex-async-ordinary-menu.txt', 'codex-async-control-hint-nonqueue.txt']) {
  test(`ordinary menu is never asyncQueue: ${name}`, () => {
    const menu = parseChoiceMenu(fx(name))
    assert.ok(menu)
    assert.equal(menu.asyncQueue, undefined)
    assert.equal(parseAsyncScreen(fx(name)), null)
    assert.ok(menu.choices.every((choice) => !typedAnswerHere(menu, choice)))
  })
}

test('別的選單裡叫 Other 的是一般選項，不是打字列', () => {
  const plain = parseChoiceMenu('Pick one?\n❯ 1. Yes\n  2. Other\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n')
  assert.ok(plain)
  assert.equal(plain.asyncQueue, undefined)
  assert.equal(typedAnswerHere(plain, plain.choices[1]), false)
})

test('多題：`1 of 2` 不黏進題目；貼字後 Other 那列長出那段字', () => {
  const two = parseChoiceMenu(OF_TWO)
  assert.ok(two)
  assert.equal(two.question, 'Pick a size?')
  assert.deepEqual(two.choices.map((c) => c.title), ['S', 'M', 'Other'])
  const typed = parseChoiceMenu(OTHER_TYPED)
  assert.ok(typed)
  assert.equal(typed.choices[2].title, 'custom ans')
  assert.equal(typed.choices[2].current, true)
})

test('自由輸入題展開後沒有編號選單：交給輸入框，不是 BlockedChoices', () => {
  assert.equal(parseChoiceMenu(FREE_TEXT), null)
})

test('panelShows：展開中、題目完整等於這一題才算；對話紀錄裡的字、切回主輸入框、別題都不算', () => {
  assert.equal(panelShows(FREE_TEXT, 'Your name?'), true)
  assert.equal(panelShows(FREE_TEXT, 'Another one?'), false, '別題')
  assert.equal(panelShows(COLLAPSED, 'Which color?'), false, '收合不算展開')
  assert.equal(panelShows('• Your name?\n› Bob\n', 'Your name?'), false, '沒有提問面板')
  assert.equal(asyncPanelText('no panel\n'), null)
  assert.ok(asyncPanelText(FREE_TEXT)?.includes('Your name?'))
})

// 複審反例（P2）：子字串比對會讓「Name?」對上「Full Name?」。
test('panelShows：題目是完整比對，不是子字串（舊題 Name? 不會對上新題 Full Name?，反之亦然）', () => {
  const panel = (q: string) => `• Queued follow-up inputs\n  ${q}\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n`
  assert.equal(panelShows(panel('Full Name?'), 'Name?'), false)
  assert.equal(panelShows(panel('Name?'), 'Full Name?'), false)
  assert.equal(panelShows(panel('Full Name?'), 'Full Name?'), true)
  // 折行的長題目：空白不計。
  assert.equal(panelShows(panel('這是很長的題目\n  折成兩行？'), '這是很長的題目折成兩行？'), true)
  // 多題的 `1 of 2` 頁碼行不是題目的一部分。
  assert.equal(panelShows(panel('1 of 2\n  Full Name?'), 'Full Name?'), true)
})

// 複審反例（P3）：輸入框原本有 Alice、貼 Bob 後是 AliceBob，只比後綴會通過。
test('panelInput：輸入框的完整內容，空的時候是佔位字；完整比對不是後綴', () => {
  const typed = (t: string) => FREE_TEXT.replace('Type your answer', t)
  assert.equal(panelInput(FREE_TEXT, 'Your name?'), EMPTY_INPUT)
  assert.equal(inputEquals(panelInput(typed('Bob'), 'Your name?'), 'Bob'), true)
  assert.equal(inputEquals(panelInput(typed('AliceBob'), 'Your name?'), 'Bob'), false, '後綴對得上但不是完整內容')
  assert.equal(inputEquals(panelInput(typed('Bob'), 'Another?'), 'Bob'), false, '別題')
  assert.equal(inputEquals(panelInput(typed('Bob Lee'), 'Your name?'), 'Bob  Lee'), true, '空白不計')
})
