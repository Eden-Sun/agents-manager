import test from 'node:test'
import assert from 'node:assert/strict'
import fixture from '../testing/fixtures/codex-1664-terminal.json'
import { splitCodexTerminalFooter } from './codexTerminalFooter'

test('真實 pane、獨立 warning 行及 Context/weekly 新段均只切出尾端 footer', () => {
  const model = 'GPT-6-Astra high · ~/project/hermes-agents/projects/rt · Context 72% used · weekly 74% left'
  for (const footer of [
    `${model}\n  ? for shortcuts\n⚠ 2 warnings · f2 to view`,
    `${model}\n  ? for shortcuts           ⚠ 2 warnings · f2 to view`,
  ]) {
    const raw = `歷史紀錄：\n${footer}\n\n• 正文不可刪除。\n\n${footer}\n\n`
    const split = splitCodexTerminalFooter(raw)
    assert.equal(split.body, `歷史紀錄：\n${footer}\n\n• 正文不可刪除。`)
    assert.deepEqual(split.footer, { modelLine: model, warnings: 2 })
  }
  assert.equal(splitCodexTerminalFooter(fixture.text).footer?.warnings, 1)
})

test('保留正文、輸入列和不在尾端的 warning；沒有 warning 時不沿用舊數字', () => {
  for (const text of ['請看 ⚠ 1 warning · f2 to view', '⚠ 1 warning · f2 to view\n後續正文', '› 草稿不可刪除']) {
    assert.deepEqual(splitCodexTerminalFooter(text), { body: text, footer: null })
  }
  const split = splitCodexTerminalFooter('› Ask Codex to do anything\nGPT-6-Astra high · ~/repo\n? for shortcuts')
  assert.equal(split.body, '› Ask Codex to do anything')
  assert.equal(split.footer?.warnings, null)
})
