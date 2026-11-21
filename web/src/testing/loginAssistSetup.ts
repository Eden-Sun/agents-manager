import { after, afterEach, before, beforeEach } from 'node:test'
import { act, fakeWebSocket, setupDom, teardownDom, unmountAll } from './domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from './sharedMock'
import { installManualTimers } from './manualTimers'
import { untilVirtual } from './untilVirtual'
import { resetStoreForTest } from '../store/store'

export function setupLoginAssistDomTest() {
  virtualMockTime()
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
    fakeWebSocket()
    ctx.timers = installManualTimers()
  })
  afterEach(async () => {
    ctx.timers!.restore()
    await unmountAll()
  })
  before(setupDom)
  const mock = sharedMock
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
