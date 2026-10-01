import test from 'node:test'
import assert from 'node:assert/strict'
import { shellDraftKey } from './shellDraft.ts'

test('shell 草稿 key 帶主機與 pane id，跟 daemon 的 shell_key 同形', () => {
  assert.equal(shellDraftKey('local', 'w1:p9'), 'shell:local/w1:p9')
  assert.notEqual(shellDraftKey('m4p', 'w1:p9'), shellDraftKey('local', 'w1:p9'))
})
