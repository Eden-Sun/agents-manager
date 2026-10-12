import test from 'node:test'
import assert from 'node:assert/strict'
import { cleanLiveText } from './liveText.ts'

test('codex idle splash does not leak into the live bubble', () => {
  const splash = `
>_ OpenAI Codex (v0.153.4)
model:        gpt-5.6-luna max  fast   /model to change
directory:    ~/project/pt
permissions:  YOLO mode
Tip: Type / to open the command popup; Tab autocompletes slash commands.
• You have 1 usage limit reset available. Run /usage to use one.
› Ask Codex to do anything
gpt-5.6-luna max fast · ~/project/pt · Context 0% used · 5h 100% left
`
  assert.equal(cleanLiveText(splash), null)
})

test('real codex prose still comes through', () => {
  assert.equal(cleanLiveText('I patched the quota parser and added a test.'), 'I patched the quota parser and added a test.')
})

test('codex warning footer stays out of live text in narrow and wide panes', () => {
  const status = '  GPT-6.1-Sol default · ~/project/agents-manager · 接手修復 GitHub issue #1486'
  for (const footer of [
    '  ? for shortcuts\n⚠ 1 warning · f2 to view',
    '  ? for shortcuts        ⚠ 2 warnings · f2 to view',
  ]) {
    assert.equal(cleanLiveText(`${status}\n${footer}`), null)
    assert.equal(cleanLiveText(`修復完成。\n${status}\n${footer}`), '修復完成。')
  }
  assert.equal(cleanLiveText('⚠ 1 warning · f2 to view'), null)
  assert.equal(cleanLiveText('⚠ 2 warnings · f2 to view'), null)
  for (const prose of ['⚠ 請先備份檔案', 'There are 2 warnings · f2 to view', '⚠ N warnings · f2 to view']) {
    assert.equal(cleanLiveText(prose), prose)
  }
})

test('spinner 完成行才刷掉：一般英文句子含「for N s」不能整句被當成 spinner', () => {
  // 刷掉的只有 `Cogitated for 5m 53s · done …`、`Thought for 12s` 這種；散文照留。
  for (const prose of ['Wait for 30s before retrying the request.', 'Retry for 5s before giving up', 'Waiting for 30s before retrying']) {
    assert.equal(cleanLiveText(prose), prose)
  }
  for (const spinner of ['Cogitated for 5m 53s · done 2:53 AM', 'Thought for 12s', 'Baked for 3m 4s']) {
    assert.equal(cleanLiveText(spinner), null, spinner)
  }
})

test('⏸ 開頭的正文不是權限模式列：只剝模式列（同 daemon claude_mode::is_mode_row）', () => {
  assert.equal(cleanLiveText('⏸ 暫停部署'), '⏸ 暫停部署')
  assert.equal(cleanLiveText('⏸ 等使用者決定，先不部署。'), '⏸ 等使用者決定，先不部署。')
  for (const row of [
    '⏸ manual mode on · ← for agents',
    '⏸ plan mode on (shift+tab to cycle) · ← for agents',
    '⏵⏵ accept edits on (shift+tab to cycle) · ← for agents',
    '⏵⏵ bypass permissions on (shift+tab to cycle)',
  ]) {
    assert.equal(cleanLiveText(row), null, row)
  }
})

test('`? for shortcuts` 後面接的正文不是 footer；footer 的右欄（至少兩格空白）照舊剝掉', () => {
  assert.equal(cleanLiveText('? for shortcuts explains the UI'), '? for shortcuts explains the UI')
  assert.equal(cleanLiveText('  ? for shortcuts'), null)
  assert.equal(cleanLiveText('  ? for shortcuts    Gemini 3.8 Flash (Medium)'), null)
  assert.equal(cleanLiveText('  ? for shortcuts        ⚠ 2 warnings · f2 to view'), null)
})

test('codex 狀態列的模型名不限於清單裡的 effort（minimal／none…），同 daemon is_codex_status_row', () => {
  for (const row of ['gpt-5.6-sol minimal · ~/p', 'gpt-5.6-sol none · ~/p', 'GPT-6.1-Sol default · ~/project/agents-manager · 接手修復 GitHub issue #1486']) {
    assert.equal(cleanLiveText(row), null, row)
  }
  // 項目符號不是模型名（#1555 在 daemon 的同一條）。
  assert.equal(cleanLiveText('- 設定檔 · ~/.codex/config.toml 已更新'), '- 設定檔 · ~/.codex/config.toml 已更新')
})

test('舊版 spinner（`⠦ Thinking… 52s`，沒有括號）是雜訊，同 daemon is_legacy_spinner', () => {
  assert.equal(cleanLiveText('⠦ Thinking… 52s'), null)
  assert.equal(cleanLiveText('好的，我來處理。\n⠦ Thinking… 52s'), '好的，我來處理。')
})
