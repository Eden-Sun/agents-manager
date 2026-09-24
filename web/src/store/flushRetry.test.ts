import test from 'node:test'
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { join } from 'node:path'
import { FLUSH_FIRST_MS, flushDelayMs, flushGaveUp, flushGaveUpText, flushMaxAttempts } from './flushRetry.ts'

test('退避：第一次是防抖，之後 1s 起加倍、封頂 30s', () => {
  assert.equal(flushDelayMs(0), FLUSH_FIRST_MS)
  assert.deepEqual([1, 2, 3, 4, 5, 6].map(flushDelayMs), [1_000, 2_000, 4_000, 8_000, 16_000, 30_000])
  // 超過表尾一律 30s，不會再往上長，也不會 undefined。
  assert.equal(flushDelayMs(7), 30_000)
  assert.equal(flushDelayMs(99), 30_000)
})

test('上限：第一次加六次重試之後停手', () => {
  assert.equal(flushMaxAttempts(), 7)
  assert.equal(flushGaveUp(6), false)
  assert.equal(flushGaveUp(7), true)
  assert.match(flushGaveUpText(7), /還留在佇列裡/)
})

// ── 整合：真的跑 store、推真的 frame（見 flushRetryBoot.harness.ts） ──

const HARNESS = join(import.meta.dirname, 'flushRetryBoot.harness.ts')

test('整合：連推 6 幀 bot_status 只送出一次、只跳一則 toast（#530）', () => {
  const r = spawnSync(process.execPath, [HARNESS], {
    env: { ...process.env, AM_FLUSH_FRAMES: '6', AM_FLUSH_GRACE_MS: '300' },
    encoding: 'utf8',
    timeout: 30_000,
  })
  assert.equal(r.status, 0, `harness failed: ${r.stderr}`)
  const out = JSON.parse(r.stdout.trim().split('\n').pop() ?? '{}') as {
    frames: number
    inWindow: number
    noticesInWindow: number
    afterRetry: number
    noticesAfterRetry: number
    distinctTexts: number
    stillQueued: boolean
  }
  // 舊行為：6 幀各排一個 350ms 的 timer，這段視窗裡會擠出 6 次 POST。
  assert.equal(out.inWindow, 1, `6 幀只該送一次，實際 ${out.inWindow} 次`)
  assert.equal(out.noticesInWindow, 1)
  // 退避之後才有第二次；同一句話不再疊第二張 toast。
  assert.equal(out.afterRetry, 2)
  assert.equal(out.noticesAfterRetry, 1, `同一則錯誤要去重，實際 ${out.noticesAfterRetry} 張`)
  assert.equal(out.distinctTexts, 1)
  assert.equal(out.stillQueued, true, '沒送出去的訊息要留在佇列裡')
})

test('整合：一路撞 409 會在上限停手，訊息留在佇列、只講一次（#530）', () => {
  const r = spawnSync(process.execPath, [HARNESS], {
    // 退避換成 10ms：真的那張表要跑 61 秒。
    env: { ...process.env, AM_FLUSH_MODE: 'cap', AM_FLUSH_FRAMES: '1' },
    encoding: 'utf8',
    timeout: 30_000,
  })
  assert.equal(r.status, 0, `harness failed: ${r.stderr}`)
  const out = JSON.parse(r.stdout.trim().split('\n').pop() ?? '{}') as {
    attempts: number
    max: number
    notices: number
    gaveUpNotices: number
    gaveUpAfterDismiss: number
    stillQueued: boolean
  }
  assert.equal(out.attempts, out.max, `送到上限就該停，實際送了 ${out.attempts} 次（上限 ${out.max}）`)
  assert.equal(out.gaveUpNotices, 1, '放棄要講一次')
  // toast 關掉之後又來幀：不可以再講第二次（這條測的是放棄旗標，不是 `notify` 的去重）。
  assert.equal(out.gaveUpAfterDismiss, 0, '放棄只講一次')
  assert.equal(out.notices, 0)
  assert.equal(out.stillQueued, true)
})
