import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { GitBar } from './GitBar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

let setProject: (id: string) => void = () => {}
function Harness() {
  const [pid, set] = useState('p1')
  setProject = set
  return <GitBar projectId={pid} showBranch />
}

function summary(branch: string) {
  return { git: true, branch, upstream: null, ahead: 0, behind: 0, changed: 0, untracked: 0, insertions: 0, deletions: 0 }
}

test('較早的 git status 晚回時，不會覆蓋切換後專案的快照', async () => {
  const realFetch = globalThis.fetch
  let finishOld: (body: string) => void = () => {}
  const oldBody = new Promise<string>((resolve) => { finishOld = resolve })
  globalThis.fetch = (async (input: string) => {
    const path = String(input)
    if (path.includes('/projects/p1/git')) return new Response(await oldBody, { status: 200 })
    if (path.includes('/projects/p2/git')) return new Response(JSON.stringify(summary('branch-two')), { status: 200 })
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch
  try {
    const host = await mount(<Harness />)
    await act(async () => setProject('p2'))
    await until(() => host.querySelector('.git-branch')?.textContent === 'branch-two', 'p2 snapshot shown')
    await act(async () => finishOld(JSON.stringify(summary('branch-one'))))
    await act(async () => new Promise((r) => setTimeout(r, 20)))
    assert.equal(host.querySelector('.git-branch')?.textContent, 'branch-two')
  } finally {
    globalThis.fetch = realFetch
  }
})
