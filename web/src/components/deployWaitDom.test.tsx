/**
 * 部署等換版窗口超過 3 分鐘（SPEC §18.10，使用者 2026-10-04）：WS 第一次通知跳 toast、擋的人換了只更新不跳；
 * header 的「⏳ 部署等 N 分」只在還在等、沒按「先等」時出現；「現在換版」「先等」打對的端點；點擋住的 bot 跳過去。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { rawTransport } from '../api/index'
import { toDeployWait } from '../api/deployWait'
import { resetStoreForTest, useStore } from '../store/store'
import { applyDeployWaitSnapshot, onDeployWaitFrame, setDeployWait, useDeployWait } from '../store/deployWait'
import { DeployWaitChip } from './DeployWaitChip'

afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
  setDeployWait(null)
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const wait = (over: Record<string, unknown> = {}) => ({
  id: 'w1',
  commit: 'abcdef1234567',
  since: new Date(Date.now() - 4.5 * 60_000).toISOString(),
  waited_secs: 240,
  blockers: [{ bot_id: 'b1', name: 'alpha', why: 'working' }],
  escalates_at: new Date(Date.now() + 26 * 60_000).toISOString(),
  user_escalated: false,
  dismissed: false,
  phase: 'waiting',
  rev: 1,
  summary: '部署 abcdef12 等換版窗口 4 分鐘了，被 alpha（working）擋住',
  ...over,
})

const btn = (label: string) => [...document.querySelectorAll('button')].find((b) => b.textContent?.trim() === label)!

test('WS：第一次通知跳一則，擋的人換了只更新，換好再跳一則', () => {
  assert.equal(onDeployWaitFrame({ wait: wait(), first: true })?.text, wait().summary)
  assert.equal(onDeployWaitFrame({ wait: wait({ rev: 2, blockers: [{ bot_id: 'b2', name: 'bravo', why: 'working' }] }), first: false }), null, '換人擋不洗版')
  assert.equal(useDeployWait.getState().wait?.blockers[0].name, 'bravo', '但 header 跟著更新')
  assert.equal(onDeployWaitFrame({ wait: wait({ rev: 3, phase: 'done', summary: '換好了' }), first: false })?.kind, 'info')
  applyDeployWaitSnapshot(undefined)
  assert.ok(useDeployWait.getState().wait, '舊 daemon 沒這欄：不動')
  applyDeployWaitSnapshot(null)
  assert.equal(useDeployWait.getState().wait, null, 'daemon 說沒有就收掉')
})

test('header：在等才出現；「現在換版」「先等」打對的端點；點擋住的 bot 跳過去', async () => {
  const calls: [string, string, unknown][] = []
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (m: string, p: string, b?: unknown) => {
    calls.push([m, p, b])
    return { ...wait(), ...(p.endsWith('escalate') ? { user_escalated: true } : { dismissed: true }) }
  }) as typeof rawTransport.request
  const selected: (string | null)[] = []
  try {
    await act(async () => useStore.setState({ notify: () => {}, selectBot: (id: string | null) => void selected.push(id) } as never))
    await act(async () => setDeployWait(toDeployWait(wait({ phase: 'swapping' }))))
    await mount(<DeployWaitChip />)
    assert.equal(document.querySelector('.deploy-wait-chip'), null, '換版中不顯示')
    await act(async () => applyDeployWaitSnapshot(wait()))
    const chip = document.querySelector('.deploy-wait-chip')!
    assert.match(chip.textContent!, /部署等 4 分/)
    await click(chip)
    assert.match(document.querySelector('.deploy-wait-pop')!.textContent!, /abcdef12.*alpha（工作中）.*自動放寬/s)
    await click(btn('alpha'))
    assert.deepEqual(selected, ['b1'], '點名字跳到那顆 bot')
    await click(chip)
    await click(btn('現在換版'))
    assert.deepEqual(calls.at(-1), ['POST', '/deploy/wait/escalate', { id: 'w1' }])
    assert.equal(useDeployWait.getState().wait?.user_escalated, true)
    await click(document.querySelector('.deploy-wait-chip')!)
    await click(btn('先等'))
    assert.deepEqual(calls.at(-1), ['POST', '/deploy/wait/dismiss', { id: 'w1' }])
    assert.equal(document.querySelector('.deploy-wait-chip'), null, '先等＝收起來')
  } finally {
    rawTransport.request = orig
  }
})
