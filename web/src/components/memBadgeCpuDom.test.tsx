import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { toMemSnapshot } from '../api/normalize'
import { resetStoreForTest, useStore } from '../store/store'

before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
afterEach(unmountAll)

const { MemBadge } = await import('./MemBadge')

test('MemBadge 與 MemPopover 顯示整機 CPU%，≥ 85% 標警示色，cpu_pct 為 null 時不顯示 CPU 且不影響記憶體', async () => {
  const realFetch = globalThis.fetch
  globalThis.fetch = (async () =>
    new Response(JSON.stringify({ host: 'local', sampled_at: new Date().toISOString(), processes: [] }), { status: 200 })) as unknown as typeof fetch
  try {
    useStore.setState({
      mem: toMemSnapshot({
        total_bytes: 2.4 * 1024 ** 3,
        herdr_bytes: 48 * 1024 ** 2,
        agents_bytes: 2.35 * 1024 ** 3,
        processes: 4,
        projects: [],
        hosts: [
          {
            host: 'local',
            herdr_bytes: 48 * 1024 ** 2,
            agents_bytes: 2.35 * 1024 ** 3,
            total_bytes: 2.4 * 1024 ** 3,
            processes: 4,
            error: null,
            browsers: [],
            machine: { total_bytes: 64 * 1024 ** 3, available_bytes: 57 * 1024 ** 3, cpu_pct: 23.4 },
          },
          {
            host: 'm4p',
            herdr_bytes: 32 * 1024 ** 2,
            agents_bytes: 3.2 * 1024 ** 3,
            total_bytes: 3.3 * 1024 ** 3,
            processes: 3,
            error: null,
            browsers: [],
            machine: { total_bytes: 16 * 1024 ** 3, available_bytes: 4 * 1024 ** 3, cpu_pct: 88.2 },
          },
          {
            host: 'nocpu',
            herdr_bytes: 16 * 1024 ** 2,
            agents_bytes: 1024 ** 3,
            total_bytes: 1024 ** 3,
            processes: 2,
            error: null,
            browsers: [],
            machine: { total_bytes: 16 * 1024 ** 3, available_bytes: 8 * 1024 ** 3, cpu_pct: null },
          },
        ],
      }),
    } as never)

    const root = await mount(
      <div>
        <MemBadge host="local" />
        <MemBadge host="m4p" />
        <MemBadge host="nocpu" />
      </div>,
    )

    const badges = root.querySelectorAll('.mem-badge')
    assert.equal(badges.length, 3)
    assert.match(badges[0]!.textContent ?? '', /RAM.*2\.4G.*剩 57G.*· CPU 23%/)
    assert.equal(badges[0]!.classList.contains('cpu-hot'), false)

    assert.match(badges[1]!.textContent ?? '', /@m4p.*3\.3G.*剩 4\.0G.*· CPU 88%/)
    assert.equal(badges[1]!.classList.contains('cpu-hot'), true)
    assert.ok(badges[1]!.querySelector('.mem-cpu.hot'))

    assert.match(badges[2]!.textContent ?? '', /@nocpu.*1\.0G.*剩 8\.0G/)
    assert.equal(badges[2]!.querySelector('.mem-cpu'), null)

    await click(root.querySelectorAll('.mem-open')[1]!)
    const popMachine = document.body.querySelector('.mem-pop-machine')
    assert.ok(popMachine)
    assert.match(popMachine.textContent ?? '', /整機 CPU：88%/)
    assert.ok(popMachine.querySelector('.mem-pop-cpu.hot'))
  } finally {
    globalThis.fetch = realFetch
  }
})
