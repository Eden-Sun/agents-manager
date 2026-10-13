/**
 * mock 的 shell 退格要跟真終端一樣「整個字元」往回刪：打一顆 emoji（兩個 UTF-16 單位）再退格，
 * 不能只砍掉後半截，留下一個落單的高位代理字元（畫面上是 �）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/

test('shell 打 emoji 再退格：輸入列不留落單代理，整顆 emoji 被刪掉', async () => {
  const mock = new MockTransport()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string }
  const pane = encodeURIComponent(shell.pane_id)
  const line = async () => {
    const t = (await mock.request('GET', `/hosts/local/shells/${pane}/terminal?source=visible&lines=5`)) as { text: string }
    return t.text.split('\n').at(-1) ?? ''
  }
  await mock.request('POST', `/hosts/local/shells/${pane}/keys`, { keys: ['a', '😀'] })
  assert.match(await line(), /a😀$/)
  await mock.request('POST', `/hosts/local/shells/${pane}/keys`, { keys: ['backspace'] })
  const after = await line()
  assert.doesNotMatch(after, LONE_SURROGATE, JSON.stringify(after))
  assert.match(after, / % a$/, JSON.stringify(after))
})
