/**
 * 分享入口回 401／403（來源被擋：Host 或 Origin 不在允許名單，見 am-share `security_headers`）：不是「連不上」，
 * 重試不會好，要停止輪詢、畫出說明，不能一直寫「連不上，正在重試…」。
 */
import test, { afterEach, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { mockShareClient } from './shareMock'
import { ShareHttpError } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'
const POLL_WAIT = 4200

for (const status of [401, 403]) {
  test(`分享入口回 ${status}：畫出「打不開」說明、停止輪詢，不寫「正在重試」`, { timeout: 20_000 }, async () => {
    const timers = installManualClockTimers()
    try {
      let calls = 0
      const base = mockShareClient(TOKEN)
      const client: ShareClient = {
        ...base,
        messages: async () => {
          calls++
          throw new ShareHttpError(status)
        },
      }
      await mount(<ShareApp client={client} />)
      await act(async () => timers.clock.advance(300))
      assert.ok(document.querySelector('.sh-gone'), '要有明確的說明畫面')
      assert.match(document.body.textContent!, /打不開/)
      assert.equal(document.querySelector('.sh-net'), null, '不能掛著「連不上，正在重試…」')
      const before = calls
      await act(async () => timers.clock.advance(POLL_WAIT))
      assert.equal(calls, before, '打不開就不再輪詢')
    } finally {
      timers.restore()
    }
  })
}
