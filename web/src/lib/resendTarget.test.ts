import test from 'node:test'
import assert from 'node:assert/strict'
import type { Message } from '../api/types'
import { resendTarget } from './resendTarget'

const message = (role: Message['role'], content: string, ids: string[] = []) => ({
  role,
  content,
  attachments: ids.map((id) => ({ id } as Message['attachments'][number])),
})

test('帶附件的訊息連 id 一起回', () => {
  assert.deepEqual(resendTarget([
    message('user', '看圖', ['a1', 'a2']),
    message('assistant', '我來看'),
  ]), { text: '看圖', attachments: ['a1', 'a2'] })
})

test('只有附件也算可以重送', () => {
  const target = resendTarget([message('user', '', ['a1'])])
  assert.notEqual(target, null)
  assert.equal(target?.text, '')
  assert.deepEqual(target?.attachments, ['a1'])
})

test('沒有 user 訊息回 null', () => {
  assert.equal(resendTarget([message('assistant', '回覆')]), null)
})

test('最後一則 user 是空的且沒附件回 null', () => {
  assert.equal(resendTarget([message('user', '上一則有效'), message('user', ' \n')]), null)
})
