/**
 * 外部使用者會遇到的錯誤文字：自動重試的錯誤要寫「正在重試」，手動的（載入較早訊息失敗）只能寫可以按重試的字，
 * 不能跟使用者說「正在重試」然後什麼都不做。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { mockShareClient } from './shareMock'
import { ShareHttpError, type ShareMessage } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

test('載入較早訊息失敗（503）：寫「連不上」並給重試鈕，不寫「正在重試」（沒有自動重試）', async () => {
  const base = mockShareClient(TOKEN)
  const recent: ShareMessage[] = [{ id: 'r1', role: 'assistant', text: '近文', created_at: '2024-01-01T00:00:00Z', attachments: [] }]
  let calls = 0
  const client: ShareClient = {
    ...base,
    messages: async (before) => {
      calls++
      if (before) throw new ShareHttpError(503)
      return { bot_name: 'b', status: 'idle', messages: recent, has_more: true }
    },
  }
  await mount(<ShareApp client={client} />)
  await settle(300)
  const older = [...document.querySelectorAll('button')].find((b) => /較早/.test(b.textContent ?? ''))!
  await click(older)
  await settle(50)
  const err = document.querySelector('.sh-send-err')
  assert.ok(err, '要有錯誤提示')
  assert.ok(!/正在重試/.test(err.textContent ?? ''), `手動的錯誤不能寫正在重試：${err.textContent}`)
  assert.ok(/連不上/.test(err.textContent ?? ''), err.textContent ?? '')
  assert.ok([...err.querySelectorAll('button')].some((b) => b.textContent === '重試'), '要有重試鈕')
  assert.ok(calls >= 2)
})
