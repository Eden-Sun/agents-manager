import test from 'node:test'
import assert from 'node:assert/strict'
import { POLL_TICK_MS, setPollTick, until } from './domHarness'

test('until 在沒有 pollTick 時行為不變：條件成立立即回傳', async () => {
  setPollTick(null)
  let calls = 0
  await until(() => {
    calls++
    return calls >= 2
  }, '兩輪後成立', 500)
  assert.equal(calls, 2)
})

test('until 在沒有 pollTick 時逾時：保持原格式錯誤訊息', async () => {
  setPollTick(null)
  await assert.rejects(
    until(() => false, '永不成立', 50),
    (err: Error) => {
      assert.equal(err.message, '等不到：永不成立')
      return true
    },
  )
})

test('until 在有 pollTick 時：條件永不成立時丟出含步數、虛擬時間與實時的錯誤', async () => {
  let virtualTime = 0
  setPollTick(async () => {
    virtualTime += POLL_TICK_MS
  })
  try {
    const ms = 250
    const maxSteps = Math.ceil(ms / POLL_TICK_MS)
    await assert.rejects(
      until(() => false, '測試條件', ms),
      (err: Error) => {
        assert.match(
          err.message,
          new RegExp(`^等不到：測試條件（輪詢 \\d+ 步、虛擬 \\d+ms、實時 \\d+ms）$`),
        )
        const match = err.message.match(/輪詢 (\d+) 步、虛擬 (\d+)ms/)
        assert.ok(match)
        const actualSteps = Number(match[1])
        assert.ok(actualSteps >= maxSteps, `步數 ${actualSteps} 必須至少達到 maxSteps ${maxSteps}`)
        return true
      },
    )
    assert.ok(virtualTime >= maxSteps * POLL_TICK_MS)
  } finally {
    setPollTick(null)
  }
})

test('until 在有 pollTick 時：慢機器每輪 settle 延遲時仍保證推進到 maxSteps（修 #1965）', async () => {
  let steps = 0
  setPollTick(async () => {
    steps++
    // 刻意每步睡 400ms（模擬真實機器延遲、慢速 CPU 讓每輪耗時大幅超過一般情況）
    await new Promise((r) => setTimeout(r, 400))
  })
  try {
    const ms = 1000 // 實時 1000ms，maxSteps = ceil(1000/250) = 4
    // 舊寫法在 1000ms 實時內只能走約 2~3 步 (<4) 就因 Date.now() < end 逾時中斷
    // 新寫法保證 steps >= maxSteps (至少 4 步)
    await assert.rejects(
      until(() => false, '步數守護測試', ms),
      (err: Error) => {
        assert.match(err.message, /輪詢 \d+ 步/)
        return true
      },
    )
    const maxSteps = Math.ceil(ms / POLL_TICK_MS)
    assert.ok(steps >= maxSteps, `慢機器上實時雖已逾時，但步數仍推進到 maxSteps (${steps} >= ${maxSteps})`)
  } finally {
    setPollTick(null)
  }
})
