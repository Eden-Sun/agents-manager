/**
 * 「⏳ 部署等 N 分」的浮層（role="dialog"）：打開時焦點進浮層，Esc 收起時焦點回到徽章鈕，不掉到 body。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, focusLikeBrowserClick, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { setDeployWait } from '../store/deployWait'
import { toDeployWait } from '../api/deployWait'
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

const wait = {
  id: 'w1',
  commit: 'abcdef1234567',
  since: new Date(Date.now() - 4.5 * 60_000).toISOString(),
  waited_secs: 270,
  blockers: [{ bot_id: 'b1', name: 'alpha', why: 'working' }],
  escalates_at: new Date(Date.now() + 26 * 60_000).toISOString(),
  user_escalated: false,
  dismissed: false,
  phase: 'waiting',
  rev: 1,
  summary: '部署 abcdef12 等換版窗口 4 分鐘了，被 alpha（working）擋住',
}

async function openChip() {
  setDeployWait(toDeployWait(wait))
  const root = await mount(<DeployWaitChip />)
  const chip = root.querySelector<HTMLButtonElement>('.deploy-wait-chip')!
  focusLikeBrowserClick(chip)
  await click(chip)
  await settle(20)
  return { chip, pop: document.querySelector<HTMLElement>('.deploy-wait-pop') }
}

test('打開後焦點進浮層（role=dialog 要有人接住焦點）', async () => {
  const { pop } = await openChip()
  assert.ok(pop, '點了要開浮層')
  assert.ok(pop.contains(document.activeElement), '焦點要在浮層裡，不能還停在徽章鈕或掉到 body')
})

test('Esc 收起浮層，焦點回到徽章鈕（不掉到 body）', async () => {
  const { chip, pop } = await openChip()
  // 前提：焦點已在浮層裡（使用者 Tab 進來或焦點進浮層）；這裡只測收起時的還焦點。
  pop!.querySelector<HTMLElement>('button')!.focus()
  await keydown(document.activeElement!, 'Escape')
  await settle(20)
  assert.equal(document.querySelector('.deploy-wait-pop'), null, 'Esc 要收起浮層')
  assert.equal(document.activeElement, chip, '焦點回到徽章鈕')
})
