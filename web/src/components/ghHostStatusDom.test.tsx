/**
 * GhHostStatus 裝置碼登入輪詢彈性審查（issue #1087）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { GhHostStatus } from './GhAuth'

before(setupDom)
after(teardownDom)
afterEach(async () => {
  await unmountAll()
})

/** 手動時鐘下的「等到條件成立」：每輪先讓 promise／React 跑完，條件還沒成立就把時鐘推進 `stepMs`（輪詢的拍子）。 */
async function untilTicking(clock: { advance(ms: number): Promise<void> }, cond: () => boolean, what: string, stepMs = 2000, maxSteps = 20) {
  for (let i = 0; i <= maxSteps; i++) {
    for (let r = 0; r < 5; r++) await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
    if (cond()) return
    await act(async () => clock.advance(stepMs))
  }
  throw new Error(`等不到：${what}`)
}

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

  // 輪詢是每 2 秒一次的 setInterval：用手動時鐘推進，不睡牆鐘。
  const { clock, restore } = installManualClockTimers()
  try {
    await mount(<GhHostStatus host="local" />)
    await untilTicking(clock, () => document.body.textContent?.includes('ABCD-1234') ?? false, '畫面出現裝置碼', 0)
    await untilTicking(clock, () => calls >= 2, '第 2 次讀取（失敗那一拍）')
    for (let r = 0; r < 5; r++) await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
    assert.ok(document.body.textContent?.includes('ABCD-1234'), '失敗後代碼面板留著')
    assert.ok(document.querySelector('.gh-auth'), '失敗後 .gh-auth 仍在')
    await untilTicking(
      clock,
      () => (document.body.textContent?.includes('gh · octocat') ?? false) && !(document.body.textContent?.includes('ABCD-1234') ?? false),
      '完成登入且代碼消失',
    )
  } finally {
    restore()
  }
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
