/**
 * mock 的兩支配對端點要跟 `daemon/src/pairing.rs` 同形（SPEC §7.1a）：
 * 碼五分鐘到期、用過即失效、比對忽略大小寫與連字號、猜錯五次限流。
 * 形狀對不上的話，`VITE_MOCK=1` 下走完的那段流程跟真 daemon 不是同一條路。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'
import { PAIR_CODE_LEN, normalizePairCode } from '../lib/pairing.ts'

const issue = async (t: MockTransport) =>
  (await t.request('POST', '/session/pair-code')) as { code: string; expires_in_secs: number; expires_at: string }

const redeem = (t: MockTransport, code: string) => t.request('POST', '/session/pair', { code })

const caught = (p: Promise<unknown>) => p.then(() => null, (e: unknown) => e)

test('產出來的碼是 ABC-DEF，帶得出到期時間', async () => {
  const t = new MockTransport()
  const got = await issue(t)

  assert.match(got.code, /^[A-Z2-9]{3}-[A-Z2-9]{3}$/)
  assert.equal(normalizePairCode(got.code).length, PAIR_CODE_LEN)
  assert.equal(got.expires_in_secs, 300)
  // 倒數要算得出來，所以 `expires_at` 必須是 parse 得動的時間。
  assert.ok(Number.isFinite(Date.parse(got.expires_at)))
})

test('拿碼換得到 token，而且用過即失效', async () => {
  const t = new MockTransport()
  const { code } = await issue(t)

  const ok = (await redeem(t, code)) as { token: string }
  assert.ok(ok.token)

  const again = await caught(redeem(t, code))
  assert.ok(again instanceof ApiError)
  assert.equal(again.status, 403)
  assert.equal(again.body.error, 'pairing_failed')
})

test('比對忽略大小寫與連字號', async () => {
  const t = new MockTransport()
  const { code } = await issue(t)

  const typed = code.toLowerCase().replace('-', ' ')
  const ok = (await redeem(t, typed)) as { token: string }
  assert.ok(ok.token)
})

test('連猜錯五次就限流，429 帶得出還要等幾秒', async () => {
  const t = new MockTransport()
  await issue(t)

  for (let i = 0; i < 4; i++) {
    const e = await caught(redeem(t, 'WRONG9'))
    assert.ok(e instanceof ApiError)
    // 前四次都還是「碼不對」，不透露已經快被鎖了。
    assert.equal(e.body.error, 'pairing_failed')
  }

  const fifth = await caught(redeem(t, 'WRONG9'))
  assert.ok(fifth instanceof ApiError)
  assert.equal(fifth.body.error, 'pairing_failed')

  const locked = await caught(redeem(t, 'WRONG9'))
  assert.ok(locked instanceof ApiError)
  assert.equal(locked.status, 429)
  assert.equal(locked.body.error, 'pairing_rate_limited')
  assert.ok(Number(locked.body.retry_after_secs) > 0)
})
