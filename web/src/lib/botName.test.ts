import { test } from 'node:test'
import assert from 'node:assert/strict'
import { isValidBotName } from './botName'
import { parseMentions, stripMentionsOf } from '../api/mentions'

test('名字中間可以有單一個空白，頭尾／連續／tab 不行（跟 daemon valid_bot_name 同規則）', () => {
  for (const ok of ['am-claude', '小幫手', 'has space', 'my bot 2', 'x'.repeat(32)]) assert.equal(isValidBotName(ok), true, ok)
  for (const bad of ['', ' ', ' lead', 'trail ', 'two  spaces', 'tab\there', 'new\nline', 'a@b', 'a,b', 'x'.repeat(33)])
    assert.equal(isValidBotName(bad), false, JSON.stringify(bad))
})

test('@ 一個有空白的名字要整個對上，最長的先比', () => {
  const ms = [{ name: 'my bot' }, { name: 'my bot 2' }, { name: 'my' }]
  const pick = (t: string) => parseMentions(t, ms).map((x) => x.name)
  assert.deepEqual(pick('@my bot 看一下'), ['my bot'])
  assert.deepEqual(pick('@My Bot，看一下'), ['my bot'])
  assert.deepEqual(pick('@my bot 2 跟 @my bot'), ['my bot', 'my bot 2'])
  assert.deepEqual(pick('@my botanist'), ['my'], '後面還接著字就不是 my bot，照舊比到 my')
  assert.deepEqual(pick('@my 自己'), ['my'])
  assert.equal(stripMentionsOf('@my bot 2 跟 @my 說', ms.map((m) => m.name)), ' 跟 @my 說', '只拿掉有空白的名字，其餘交給原本的規則')
})
