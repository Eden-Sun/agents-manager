import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { Profiler, useState } from 'react'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { IssuesBar } from './IssuesBar'

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const issue = (number: number, title: string) => ({ number, title, state: 'open', labels: [], url: `https://github.com/example/x/issues/${number}`, updated_at: '2026-10-10T00:00:00Z', author: 'u', body_excerpt: '' })

test('IssuesBar：切到另一個專案，第一幀不顯示舊專案的未結案數', async () => {
  const originalFetch = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    const path = String(input)
    const body = path.includes('/projects/p1/')
      ? { issues: [issue(1, 'OLD-PROJECT issue')] }
      : path.includes('/projects/p2/')
        ? { issues: [issue(2, 'NEW-PROJECT one'), issue(3, 'NEW-PROJECT two')] }
        : {}
    return new Response(JSON.stringify(body), { status: 200 })
  }) as typeof fetch
  try {
    resetStoreForTest()
    const repo = (id: string, name: string) => ({ id, label: name, path: `/${id}`, host: 'local', github: { owner: 'example', repo: name, url: `https://github.com/example/${name}` } })
    useStore.setState({ projects: [repo('p1', 'old'), repo('p2', 'new')], drafts: {} } as never)
    const frames: string[] = []
    let setId: (id: string) => void = () => {}
    const read = () => `${document.querySelector('.issues-repo')?.textContent ?? ''}|${document.querySelector('.issues-count')?.textContent ?? ''}`
    function Harness() {
      const [id, set] = useState('p1')
      setId = set
      return (
        <Profiler id="issues" onRender={() => frames.push(read())}>
          <IssuesBar projectId={id} draftKey={`bot:${id}`} inputRef={{ current: null }} />
        </Profiler>
      )
    }
    await mount(<Harness />)
    await act(async () => click(document.querySelector('.issues-btn')!))
    for (let i = 0; i < 200 && !read().includes('1 open'); i++) {
      await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
    }
    assert.ok(read().includes('1 open'), `切換前應已載入舊專案的計數：${read()}`)
    frames.length = 0
    await act(async () => setId('p2'))
    for (let i = 0; i < 200 && !read().includes('2 open'); i++) {
      await act(async () => new Promise<void>((resolve) => setImmediate(resolve)))
    }
    assert.ok(frames.length > 0)
    assert.ok(frames.every((f) => !f.includes('1 open')), `切換後的 commit 出現舊專案的未結案數：${JSON.stringify(frames)}`)
    assert.equal(read(), 'example/new|2 open')
  } finally {
    globalThis.fetch = originalFetch
  }
})
