/**
 * GhHostStatus 裝置碼登入輪詢彈性審查（issue #1087）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { GhHostStatus } from './GhAuth'

before(setupDom)
after(teardownDom)
afterEach(async () => {
  await unmountAll()
})

test('輪詢中一次讀失敗：代碼面板留著、之後繼續輪詢到登入完成', async () => {
  let calls = 0
  fakeApi((req) => {
    if (req.path.includes('/hosts/local/gh')) {
      calls++
      if (calls === 1) {
        return {
          name: 'local',
          installed: true,
          logged_in: false,
          pending: {
            user_code: 'ABCD-1234',
            verification_uri: 'https://github.com/login/device',
            verification_uri_complete: null,
            expires_in: 900,
          },
        }
      }
      if (calls === 2) {
        throw new Error('boom')
      }
      return {
        name: 'local',
        installed: true,
        logged_in: true,
        account: 'octocat',
        pending: null,
      }
    }
    return undefined
  })

  await mount(<GhHostStatus host="local" />)
  await until(() => document.body.textContent?.includes('ABCD-1234') ?? false, '畫面出現裝置碼')
  await until(() => calls >= 2, '第 2 次讀取（失敗那一拍）')
  assert.ok(document.body.textContent?.includes('ABCD-1234'), '失敗後代碼面板留著')
  assert.ok(document.querySelector('.gh-auth'), '失敗後 .gh-auth 仍在')
  await until(
    () => (document.body.textContent?.includes('gh · octocat') ?? false) && !(document.body.textContent?.includes('ABCD-1234') ?? false),
    '完成登入且代碼消失',
    8000,
  )
})

test('第一次就讀不到：不畫東西、不丟例外', async () => {
  fakeApi((req) => {
    if (req.path.includes('/hosts/local/gh')) {
      throw new Error('boom')
    }
    return undefined
  })

  await mount(<GhHostStatus host="local" />)
  await settle()
  assert.equal(document.querySelector('.gh-auth'), null)
})
