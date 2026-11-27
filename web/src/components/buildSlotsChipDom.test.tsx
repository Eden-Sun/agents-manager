import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { handleFrame, resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import type { BuildSlots } from '../api/buildSlots'
import { resetBuildSlotsForTest } from '../store/buildSlots'
import { BuildSlotsChip } from './BuildSlotsChip'

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

before(() => {
  resetBuildSlotsForTest()
  return setupDom()
})
afterEach(async () => {
  await unmountAll()
  resetBuildSlotsForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const ago = (s: number) => new Date(Date.now() - s * 1000).toISOString()

const idle: BuildSlots = { max_concurrent: 2, active: 0, slots: [], remote: { enabled: false, hosts: [] }, remote_runs: [], total_active: 0, total_waiting: 0 }
const busy: BuildSlots = {
  max_concurrent: 2,
  active: 1,
  slots: [{ holder: 'b:1', status: 'held', bot_id: 'bot-1', purpose: 'test -p agents-managerd', host: 'local', since: ago(75) }],
  remote: { enabled: true, hosts: [{ target: 'me@box:22', local: false, active: 2, waiting: 0 }, { target: 'me@down:22', local: false, active: null, waiting: null }] },
  remote_runs: [{ call_id: 'C1', bot_id: 'bot-1', purpose: 'clippy --workspace', since: ago(30), target: 'me@box:22' }],
  total_active: 3,
  total_waiting: 0,
}
const queued: BuildSlots = { ...busy, total_waiting: 2, slots: [...busy.slots, { holder: 'm:2', status: 'waiting', bot_id: null, purpose: 'build', host: 'local', since: ago(5) }] }

async function serving<T>(snap: BuildSlots, fn: () => Promise<T>): Promise<{ result: T; calls: number }> {
  const orig = rawTransport.request.bind(rawTransport)
  let calls = 0
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (method === 'GET' && path === '/build-slots') {
      calls += 1
      return snap
    }
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try {
    return { result: await fn(), calls }
  } finally {
    rawTransport.request = orig
  }
}

const chip = () => document.querySelector<HTMLElement>('[data-k="build-slots-chip"]')

it('0 個建置：淡色「建置 0」', async () => {
  resetStoreForTest()
  await serving(idle, async () => {
    await mount(<BuildSlotsChip />)
    await until(() => chip() !== null, '晶片畫出來')
    assert.match(chip()!.textContent ?? '', /建置 0/)
    assert.ok(chip()!.classList.contains('idle'))
  })
})

it('有建置：數字是本機＋遠端合計，tooltip 列出每格與每台', async () => {
  resetStoreForTest()
  useStore.setState({ bots: [{ id: 'bot-1', name: 'reviewer' }] } as never)
  await serving(busy, async () => {
    await mount(<BuildSlotsChip />)
    await until(() => chip() !== null, '晶片畫出來')
    const c = chip()!
    assert.match(c.textContent ?? '', /建置 3/)
    assert.ok(!c.classList.contains('idle'))
    const tip = c.title
    assert.match(tip, /本機 · test -p agents-managerd · reviewer · 1m1\d/)
    assert.match(tip, /me@box:22 · clippy --workspace · reviewer · 3\ds/)
    assert.match(tip, /me@down:22：讀不到名額/)
    assert.match(tip, /me@box:22：2 個進行中、0 個排隊/)
  })
})

it('有排隊：顯示「建置 N＋M 排隊」，推送會即時換數字', async () => {
  resetStoreForTest()
  await serving(queued, async () => {
    await mount(<BuildSlotsChip />)
    await until(() => chip() !== null, '晶片畫出來')
    assert.match(chip()!.textContent ?? '', /建置 3＋2 排隊/)
    assert.ok(chip()!.classList.contains('queued'))
    handleFrame(useStore.setState, useStore.getState, { type: 'build_slots_updated', data: { ...idle, total_active: 1, slots: [{ ...busy.slots[0] }] } })
    await until(() => /建置 1/.test(chip()!.textContent ?? ''), '推送後換成 1')
    assert.ok(!chip()!.classList.contains('queued'))
  })
})

it('讀不到 /api/build-slots：不畫晶片', async () => {
  resetStoreForTest()
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async () => {
    throw new Error('down')
  }) as typeof rawTransport.request
  try {
    await mount(<BuildSlotsChip />)
    await new Promise((r) => setTimeout(r, 50))
    assert.equal(chip(), null)
  } finally {
    rawTransport.request = orig
  }
})
