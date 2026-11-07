/**
 * #1242：GitBar 在分頁背景時不抓 git 狀態（遠端專案每次一趟 ssh），回前景立刻補一次（同 #909／#1129 的輪詢）。
 * 真的掛 `GitBar` 進 happy-dom；15 秒一輪用手動時鐘推。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { installManualClockTimers } from '../testing/manualClockTimers'
import { GitBar } from './GitBar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

function setVisibility(state: 'hidden' | 'visible') {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state })
}

test('背景時不抓 git 狀態，回前景立刻補一次', async () => {
  const requests = fakeApi(() => ({ git: false }))
  const gitReads = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/projects/p1/git')).length
  setVisibility('hidden')
  const timers = installManualClockTimers()
  try {
    await mount(<GitBar projectId="p1" />)
    await act(async () => timers.clock.advance(16_000))
    await act(async () => timers.clock.advance(16_000))
    assert.equal(gitReads(), 0, '背景：15 秒一輪也不抓')

    setVisibility('visible')
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    assert.equal(gitReads(), 1, '回前景：立刻補一次，不必等下一個 tick')
  } finally {
    timers.restore()
    setVisibility('visible')
  }
})

test('前景時掛上就抓一次，之後照 15 秒一輪', async () => {
  const requests = fakeApi(() => ({ git: false }))
  const gitReads = () => requests.filter((r) => r.method === 'GET' && r.path.includes('/projects/p1/git')).length
  setVisibility('visible')
  const timers = installManualClockTimers()
  try {
    await mount(<GitBar projectId="p1" />)
    assert.equal(gitReads(), 1, '前景掛上立刻抓')
    await act(async () => timers.clock.advance(15_000))
    assert.equal(gitReads(), 2, '前景照常輪詢')
  } finally {
    timers.restore()
  }
})


test('Git 動作預設保留，手機可只顯示分支與狀態', async () => {
  fakeApi(() => ({ git: true, branch: 'main', upstream: 'origin/main', changed: 1, insertions: 0, deletions: 0, untracked: 8, ahead: 0, behind: 1 }))
  setVisibility('visible')
  await mount(<><GitBar projectId="desktop" /><GitBar projectId="phone" showBranch showActions={false} /></>)
  await until(() => document.querySelectorAll('.git-bar').length === 2, '兩個 Git 狀態載入')
  const [desktop, phone] = Array.from(document.querySelectorAll('.git-bar'))
  assert.equal(desktop!.querySelectorAll('button').length, 3, '桌機預設保留 commit／push／pull')
  assert.equal(phone!.querySelector('button'), null)
  assert.equal(phone!.querySelector('.git-actions'), null)
  assert.equal(phone!.querySelector('.git-branch')?.textContent, 'main')
  assert.match(phone!.textContent ?? '', /main\+0−0\?8↓1/)
})
