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
