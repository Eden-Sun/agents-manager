import test from 'node:test'
import assert from 'node:assert/strict'
import fixture from '../testing/fixtures/codex-1664-terminal.json'
import fixture1671 from '../testing/fixtures/codex-1671-terminal.json'
import { parseCodexModelLine, splitCodexTerminalFooter } from './codexTerminalFooter'
import context1755 from '../testing/fixtures/codex-1755-terminal-context.json'
import shortcuts1755 from '../testing/fixtures/codex-1755-terminal-shortcuts.json'

test('真實 pane、獨立 warning 行及 Context/weekly 新段均只切出尾端 footer', () => {
  const model = 'GPT-6-Astra high · ~/project/hermes-agents/projects/rt · Context 72% used · weekly 74% left'
  for (const footer of [
    `${model}\n  ? for shortcuts\n⚠ 2 warnings · f2 to view`,
    `${model}\n  ? for shortcuts           ⚠ 2 warnings · f2 to view`,
  ]) {
    const raw = `歷史紀錄：\n${footer}\n\n• 正文不可刪除。\n\n${footer}\n\n`
    const split = splitCodexTerminalFooter(raw)
    // 終端畫面照 pane 原樣顯示：footer 留在 body 裡，只另外解析給狀態列用（#1680）。
    assert.equal(split.body, raw)
    assert.deepEqual(split.footer, { modelLine: model, warnings: 2 })
  }
  assert.equal(splitCodexTerminalFooter(fixture.text).footer?.warnings, 1)
})

test('保留正文、輸入列和不在尾端的 warning；沒有 warning 時不沿用舊數字', () => {
  for (const text of ['請看 ⚠ 1 warning · f2 to view', '⚠ 1 warning · f2 to view\n後續正文', '› 草稿不可刪除']) {
    assert.deepEqual(splitCodexTerminalFooter(text), { body: text, footer: null })
  }
  const split = splitCodexTerminalFooter('› Ask Codex to do anything\nGPT-6-Astra high · ~/repo\n? for shortcuts')
  assert.equal(split.body, '› Ask Codex to do anything\nGPT-6-Astra high · ~/repo\n? for shortcuts')
  assert.equal(split.footer?.warnings, null)
})

test('#1671 真實快照：warning 直接接在模型列右側（沒有 ? for shortcuts 行）也切進狀態列', () => {
  const split = splitCodexTerminalFooter(fixture1671.text)
  assert.deepEqual(split.footer, {
    modelLine: 'GPT-6.1-Sol high · ~/project/hermes-agents/projects/rt · Context 17% used · weekly 100% left',
    warnings: 1,
  })
  assert.equal(split.body, fixture1671.text)
})

test('#1671 模型列右側 warning：複數與沒有 warning', () => {
  const model = 'GPT-6.1-Sol high · ~/repo · Context 5% used · 5h 96% left · weekly 48% left'
  const plural = splitCodexTerminalFooter(`• 完成\n\n› Ask Codex to do anything\n\n  ${model}      ⚠ 3 warnings · f2 to view\n`)
  assert.deepEqual(plural.footer, { modelLine: model, warnings: 3 })
  assert.equal(plural.body, `• 完成\n\n› Ask Codex to do anything\n\n  ${model}      ⚠ 3 warnings · f2 to view\n`)
  const none = splitCodexTerminalFooter(`› Ask Codex to do anything\n\n  ${model}\n`)
  assert.deepEqual(none.footer, { modelLine: model, warnings: null })
})

test('只有 `? for shortcuts` 這行（沒有模型列、沒有 warning）不算 footer：它是 chrome，不是狀態欄位', () => {
  // 空 footer 會讓 StatusLineBar 畫出一條空的狀態列、並蓋掉 status_line 的文字。
  const text = '• 正文\n  ? for shortcuts'
  assert.deepEqual(splitCodexTerminalFooter(text), { body: text, footer: null })
})

test('#1755 模型列拆成欄位：模型、effort、fast、目錄、context、額度，其餘是主題', () => {
  const ctx = splitCodexTerminalFooter(context1755.text).footer!.modelLine!
  assert.deepEqual(parseCodexModelLine(ctx), {
    model: 'GPT-6.1-Sol', effort: 'low', fast: false, cwd: '~/project/hermes-agents/projects/rt',
    contextUsedPct: 18, limits: [{ label: 'weekly', leftPct: 78 }], title: null,
  })
  const topic = splitCodexTerminalFooter(shortcuts1755.text).footer!.modelLine!
  assert.deepEqual(parseCodexModelLine(topic), {
    model: 'GPT-6.1-Sol', effort: 'low', fast: false, cwd: '~/project/agm-lx1685',
    contextUsedPct: null, limits: [], title: '接手並完成 issue #1685',
  })
  assert.deepEqual(parseCodexModelLine('GPT-6.1-Sol high fast · ~/repo · Context 5% used · 5h 96% left · weekly 48% …'), {
    model: 'GPT-6.1-Sol', effort: 'high', fast: true, cwd: '~/repo', contextUsedPct: 5,
    limits: [{ label: '5h', leftPct: 96 }, { label: 'weekly', leftPct: 48 }], title: null,
  })
  assert.equal(parseCodexModelLine('GPT-6-Astra default · ~/x · 主題 · 含 · 分隔').title, '主題 · 含 · 分隔')
})
