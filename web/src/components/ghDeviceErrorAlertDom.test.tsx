import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { GhHostStatus } from './GhAuth'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

test('裝置碼登入失敗：錯誤文字是 role=alert', async () => {
  fakeApi((req) => {
    if (req.path.includes('/hosts/local/gh')) {
      return { name: 'local', installed: true, logged_in: false, account: null, pending: null, error: '裝置碼已過期，請重新登入' }
    }
    return undefined
  })
  await mount(<GhHostStatus host="local" />)
  await until(() => document.querySelector('.gh-device-err') !== null, '畫面出現錯誤')
  assert.equal(document.querySelector('.gh-device-err')!.getAttribute('role'), 'alert')
})
