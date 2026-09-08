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
