/**
 * Jev 帳本的節流補抓（#1378）：60 秒內來的 `supervisor_changed` 不丟掉，滿 60 秒那一刻補抓一次。
 * 時間全部由 `manualClock` 注入：`refresh({ now: clock.now() })` 與 `setTimeout` 走同一把虛擬時鐘，不靠牆鐘、不靠 sleep。
 */
import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { resetJudgeLedgerForTest, useJudgeLedger } from './judgeLedger'

before(setupDom)
after(() => {
  resetJudgeLedgerForTest()
  teardownDom()
})

/** 虛擬時鐘從 0 起跑，節流的起點（`lastFetchAt` 預設 0）要先推開，不然第一次就被當成節流中。 */
const START = 1_000_000

const row = (id: string) => ({
  id,
  bot_id: 'bot-a',
  run_id: null,
  kind: null,
  matched_line: null,
  composer_idle: null,
  jev_is_live_ui: null,
  jev_same_work: null,
  model: null,
  ms: null,
  input_tokens: null,
  error: null,
  cleared_at: null,
  assignment_id: null,
  claims_verified: null,
  asks_parent_action: null,
  regex_verdict: 'stuck_queued',
  at: new Date(START).toISOString(),
})

/**
 * `/judge/shadow` 的替身：`hits` 記下被打幾次；`rows` 是下一次回的內容；
 * `gate` 有值時要等它 resolve 才回（模擬「抓到一半」）。
 */
function stubShadow() {
  const state = { hits: 0, rows: [row('first')] as unknown[], gate: null as Promise<void> | null }
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    if (path.startsWith('/judge/shadow')) {
      state.hits += 1
      if (state.gate) await state.gate
      return new Response(JSON.stringify({ enabled: true, projects: [], model: 'jev-1.13.0', rows: state.rows }), { status: 200 })
    }
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  return state
}

/** 讓已排好的 microtask／setImmediate 跑完（不走 setTimeout，所以手動時鐘下也能用）。 */
async function drain(rounds = 30) {
  for (let i = 0; i < rounds; i++) await new Promise<void>((resolve) => setImmediate(resolve))
}

test('節流期間到的事件在 60 秒滿時補抓一次，補抓回來的內容進了 store', async () => {
  const stub = stubShadow()
  const { clock, restore } = installManualClockTimers()
  try {
    resetJudgeLedgerForTest()
    await clock.advance(START)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    assert.equal(stub.hits, 1)

    await clock.advance(4_000)
    stub.rows = [row('late')]
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    await drain()
    assert.equal(stub.hits, 1, '節流中不立刻打')

    await clock.advance(55_999)
    await drain()
    assert.equal(stub.hits, 1, '還差 1 毫秒不補抓')

    await clock.advance(1)
    await drain()
    assert.equal(stub.hits, 2, '滿 60 秒那一刻補抓一次')
    assert.deepEqual(useJudgeLedger.getState().rows.map((r) => r.id), ['late'])
  } finally {
    resetJudgeLedgerForTest()
    restore()
  }
})

test('節流期間來三次事件，滿 60 秒只補抓一次', async () => {
  const stub = stubShadow()
  const { clock, restore } = installManualClockTimers()
  try {
    resetJudgeLedgerForTest()
    await clock.advance(START)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    for (const gap of [1_000, 2_000, 3_000]) {
      await clock.advance(gap)
      await useJudgeLedger.getState().refresh({ now: clock.now() })
    }
    await clock.advance(60_000)
    await drain()
    assert.equal(stub.hits, 2, '三次事件合成一次補抓')
  } finally {
    resetJudgeLedgerForTest()
    restore()
  }
})

test('抓到一半進來的事件，抓完後補一次', async () => {
  const stub = stubShadow()
  let release!: () => void
  stub.gate = new Promise<void>((resolve) => {
    release = resolve
  })
  const { clock, restore } = installManualClockTimers()
  try {
    resetJudgeLedgerForTest()
    await clock.advance(START)
    const first = useJudgeLedger.getState().refresh({ now: clock.now() })
    await clock.advance(1_000)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    assert.equal(stub.hits, 1, '抓到一半時不再開第二趟')

    release()
    await first
    await drain()
    // 時鐘已經在 START+1000（最後一次要求的時間），補抓要到 START+60000 才跑：還差 1 毫秒不跑。
    await clock.advance(58_999)
    await drain()
    assert.equal(stub.hits, 1, '補抓排在第一趟開始後滿 60 秒')

    await clock.advance(1)
    await drain()
    assert.equal(stub.hits, 2, '抓完後補抓一次')
  } finally {
    resetJudgeLedgerForTest()
    restore()
  }
})

test('force 會取消排著的補抓', async () => {
  const stub = stubShadow()
  const { clock, restore } = installManualClockTimers()
  try {
    resetJudgeLedgerForTest()
    await clock.advance(START)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    await clock.advance(4_000)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    await useJudgeLedger.getState().refresh({ now: clock.now(), force: true })
    await drain()
    assert.equal(stub.hits, 2, 'force 立刻重抓')

    await clock.advance(60_000)
    await drain()
    assert.equal(stub.hits, 2, '排著的那一次已經被 force 取消')
  } finally {
    resetJudgeLedgerForTest()
    restore()
  }
})

test('沒有被擋下的事件就不排補抓', async () => {
  const stub = stubShadow()
  const { clock, restore } = installManualClockTimers()
  try {
    resetJudgeLedgerForTest()
    await clock.advance(START)
    await useJudgeLedger.getState().refresh({ now: clock.now() })
    await clock.advance(120_000)
    await drain()
    assert.equal(stub.hits, 1, '只有一次要求，之後沒有補抓')
  } finally {
    resetJudgeLedgerForTest()
    restore()
  }
})
