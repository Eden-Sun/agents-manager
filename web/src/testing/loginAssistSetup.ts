// claude／codex 登入協助 DOM 測試共用的骨架：虛擬時鐘（輪詢與 mock timer 一起推）、假 WebSocket、DOM 生命週期，
// 以及共用 mock 的 `localIdentityStatus` 還原（同一行程只有一個 mock，前一個檔登入的身份不能漏到下一個檔）。
import { after, afterEach, before, beforeEach } from 'node:test'
import { act, fakeWebSocket, setupDom, teardownDom, unmountAll } from './domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from './sharedMock'
import { installManualTimers } from './manualTimers'
import { untilVirtual } from './untilVirtual'
import { resetStoreForTest } from '../store/store'

/** 在測試檔頂層呼叫一次：註冊 hooks，回傳 `mock`、`until`（等條件成立，推進虛擬時間）與 `elapse`（讓輪詢與 mock 走 `ms` 虛擬毫秒）。 */
export function setupLoginAssistDomTest() {
  virtualMockTime()
  // HostShellPanel 的輪詢（250ms／1s 一次）走手動時鐘，等待只推進虛擬時間；mock 的 timer 在 `virtualMockTime` 的時鐘上，一起推。
  const ctx: { timers: ReturnType<typeof installManualTimers> | null } = { timers: null }
  const clock = () => ctx.timers!.clock
  const until = (cond: () => boolean | Promise<boolean>, what: string) =>
    untilVirtual(clock(), cond, what, { stepMs: 250, alsoAdvance: advanceMockTime })
  const elapse = (ms: number) =>
    act(async () => {
      await clock().advance(ms)
      await advanceMockTime(ms)
    })
  beforeEach(() => {
    // 桌面直通會開 xterm 的推送 WebSocket；假掉，免得真的去連。
    fakeWebSocket()
    ctx.timers = installManualTimers()
  })
  afterEach(async () => {
    ctx.timers!.restore() // unmountAll 的 settle 要真 timer
    await unmountAll()
  })
  before(setupDom)
  const mock = sharedMock
  // 登入 cc1 會在共用 mock 上留下「已登入」；整個行程只有一個 mock，後面要「cc1 未登入」前提的檔（settingsIdentitiesDom）會被它弄紅。
  type MockIds = { localIdentityStatus: Record<string, unknown> }
  const idsBefore = structuredClone((mock as unknown as MockIds).localIdentityStatus)
  after(() => {
    const m = mock as unknown as MockIds
    for (const k of Object.keys(m.localIdentityStatus)) delete m.localIdentityStatus[k]
    Object.assign(m.localIdentityStatus, structuredClone(idsBefore))
    resetStoreForTest()
    teardownDom()
  })
  return { mock, until, elapse }
}
