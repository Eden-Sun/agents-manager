import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { handleFrame, resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import type { BuildSlots } from '../api/buildSlots'
import { resetBuildSlotsForTest } from '../store/buildSlots'
import { BuildSlotsChip } from './BuildSlotsChip'
import { HeadMemBadges } from './HeadMemBadges'

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
const shown = () => {
  const c = chip()!
  return `${c.querySelector('.mem-k')?.textContent} ${c.querySelector('.mem-v')?.textContent}`
}

it('0 個建置：淡色「建置 0」，與 RAM 徽章同一套 mem-badge 外觀', async () => {
  resetStoreForTest()
  await serving(idle, async () => {
    await mount(<BuildSlotsChip />)
    await until(() => chip() !== null, '晶片畫出來')
    assert.equal(shown(), '建置 0')
    assert.ok(chip()!.classList.contains('mem-badge'))
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
    assert.equal(shown(), '建置 3')
    assert.ok(!c.classList.contains('idle'))
    const tip = c.title
    assert.match(tip, /本機 · test -p agents-managerd · reviewer · 1m1\d/)
    assert.match(tip, /me@box:22 · clippy --workspace · reviewer · 3\ds/)
    assert.match(tip, /me@down:22：讀不到名額/)
    assert.match(tip, /me@box:22：2 個進行中、0 個排隊/)
  })
})

it('有排隊：顯示「建置 N＋M」，推送會即時換數字', async () => {
  resetStoreForTest()
  await serving(queued, async () => {
    await mount(<BuildSlotsChip />)
    await until(() => chip() !== null, '晶片畫出來')
    assert.equal(shown(), '建置 3＋2')
    assert.equal(chip()!.getAttribute('aria-label'), '建置 3＋2 排隊')
    assert.ok(chip()!.classList.contains('queued'))
    handleFrame(useStore.setState, useStore.getState, { type: 'build_slots_updated', data: { ...idle, total_active: 1, slots: [{ ...busy.slots[0] }] } })
    await until(() => shown() === '建置 1', '推送後換成 1')
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

it('左上主機數據區：只有本機與有遠端主機時，晶片都跟 RAM 徽章放在同一區', async () => {
  resetStoreForTest()
  const row = (host: string) => ({ host, total_bytes: 1 << 30, herdr_bytes: 1 << 20, agents_bytes: 1 << 29, processes: 3, browsers: [], machine: null, error: null })
  await serving(busy, async () => {
    useStore.setState({ mem: { hosts: [row('local')] } } as never)
    await mount(<HeadMemBadges />)
    await until(() => chip() !== null, '單機時畫出來')
    assert.ok(chip()!.parentElement!.classList.contains('head-ram'))
    await act(() => useStore.setState({ mem: { hosts: [row('local'), row('m4p')] } } as never))
    await until(() => document.querySelector('.head-ram.multi') !== null, '換成多主機')
    assert.equal(document.querySelectorAll('[data-k="build-slots-chip"]').length, 1)
    assert.ok(chip()!.parentElement!.classList.contains('multi'))
    assert.ok(document.querySelector('[data-k="head-hosts"]'), '多主機收成一顆摘要膠囊，與建置晶片同一列')
  })
})
