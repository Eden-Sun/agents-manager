/** #1833：repo／query 切換的 debounce 空窗不能讓舊 repo 搜尋結果重新出現或被插入。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useRef } from 'react'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { IssuesBar } from './IssuesBar'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const SUB_PATH = 'packages/widget'
const MAIN_ISSUE = {
  number: 7,
  title: 'MAIN issue seven',
  state: 'open',
  labels: [],
  url: 'https://github.com/example/main/issues/7',
  updated_at: '2026-10-10T00:00:00Z',
  author: 'main-user',
  body_excerpt: '',
}
const SUB_ISSUE = {
  ...MAIN_ISSUE,
  title: 'SUB issue seven',
  url: 'https://github.com/example/sub/issues/7',
  author: 'sub-user',
}

function respond(value: unknown): Response {
  return new Response(JSON.stringify(value), { status: 200 })
}

function setupProject() {
  resetStoreForTest()
  useStore.setState({
    projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local', github: { owner: 'example', repo: 'main', url: 'https://github.com/example/main' } }] as never,
    drafts: {},
  })
}

function Harness() {
  const ref = useRef<HTMLTextAreaElement>(null)
  return (
    <>
      <textarea ref={ref} />
      <IssuesBar projectId="p1" draftKey="bot:b1" inputRef={ref} />
    </>
  )
}

function installApi({ deferDetails = false }: { deferDetails?: boolean } = {}) {
  const originalFetch = globalThis.fetch
  const requests: string[] = []
  const mainSearches: { resolve: (response: Response) => void; reject: (error: unknown) => void }[] = []
  const details: { repo: string; resolve: (response: Response) => void }[] = []
  globalThis.fetch = ((input: RequestInfo | URL) => {
    const path = String(input)
    requests.push(path)
    const url = new URL(path, location.origin)
    if (url.pathname.endsWith('/session')) return Promise.resolve(respond({ token: 'test-token' }))
    if (url.pathname.endsWith('/submodules')) {
      return Promise.resolve(respond({ submodules: [{ path: SUB_PATH, github: { owner: 'example', repo: 'sub', url: 'https://github.com/example/sub' } }] }))
    }
    if (/\/issues\/7$/.test(url.pathname)) {
      const repo = url.searchParams.get('repo') ?? ''
      if (deferDetails) return new Promise<Response>((resolve) => details.push({ repo, resolve }))
      const issue = repo === SUB_PATH ? SUB_ISSUE : MAIN_ISSUE
      const body = repo === SUB_PATH ? 'SUB repository body' : 'MAIN repository body'
      return Promise.resolve(respond({ issue: { ...issue, body } }))
    }
    if (url.pathname.endsWith('/issues')) {
      const query = url.searchParams.get('q')
      const repo = url.searchParams.get('repo') ?? ''
      if (query === 'needle' && !repo) {
        return new Promise<Response>((resolve, reject) => mainSearches.push({ resolve, reject }))
      }
      if (query === 'needle' && repo === SUB_PATH) return Promise.resolve(respond({ issues: [SUB_ISSUE] }))
      return Promise.resolve(respond({ issues: [] }))
    }
    return Promise.resolve(respond({}))
  }) as typeof fetch
  return { requests, mainSearches, details, restore: () => { globalThis.fetch = originalFetch } }
}

async function startMainSearch() {
  await mount(<Harness />)
  await click(document.querySelector('.issues-btn')!)
  await settle(10)
  const search = document.querySelector<HTMLInputElement>('input[type="search"]')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(search), 'value')!.set!
    set.call(search, 'needle')
    search.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await settle(350)
  assert.equal(document.querySelector('.issue-title'), null, '主 repo 的搜尋尚未回應')
}

async function switchRepo(repo: string) {
  const select = document.querySelector<HTMLSelectElement>('.issues-repo-pick')!
  await act(async () => {
    select.value = repo
    select.dispatchEvent(new Event('change', { bubbles: true }))
  })
}

test('切 repo 後 debounce 期間丟棄舊結果，完整插入使用新 repo 的標題、URL 與內文', async () => {
  setupProject()
  const api = installApi()
  try {
    await startMainSearch()
    assert.equal(api.mainSearches.length, 1, '主 repo 的 needle 請求仍在飛')

    await switchRepo(SUB_PATH)
    await act(async () => api.mainSearches[0].resolve(respond({ issues: [MAIN_ISSUE] })))
    await settle(0)
    assert.equal(document.querySelector('.issue-title'), null, '舊 repo 回應不能填回新 repo 的列表')

    await settle(350)
    assert.equal(document.querySelector('.issue-title')?.textContent?.trim(), '#7 SUB issue seven')
    await click(document.querySelector('.issues-list .mini-btn.primary')!)
    await settle(20)
    const draft = useStore.getState().drafts['bot:b1'] ?? ''
    assert.match(draft, /#7 \(https:\/\/github\.com\/example\/sub\/issues\/7\)/)
    assert.match(draft, /> SUB issue seven\n> SUB repository body/)
    assert.doesNotMatch(draft, /MAIN issue seven|MAIN repository body/)
    assert.ok(api.requests.some((path) => path.includes('/issues/7?repo=packages%2Fwidget')), 'detail 使用 submodule repo')
  } finally {
    api.restore()
  }
})

test('切 repo 後舊搜尋失敗不覆蓋新 repo 狀態', async () => {
  setupProject()
  const api = installApi()
  try {
    await startMainSearch()
    assert.equal(api.mainSearches.length, 1)

    await switchRepo(SUB_PATH)
    await act(async () => api.mainSearches[0].reject(new TypeError('stale main search failed')))
    await settle(0)
    assert.equal(document.querySelector('.issues-status.err'), null, '舊 repo 錯誤不能顯示在新 repo 下')

    await settle(350)
    assert.equal(document.querySelector('.issue-title')?.textContent?.trim(), '#7 SUB issue seven')
  } finally {
    api.restore()
  }
})

test('切 repo 後，已在讀取的舊 issue 內容不再插入草稿', async () => {
  setupProject()
  const api = installApi({ deferDetails: true })
  try {
    await startMainSearch()
    await switchRepo(SUB_PATH)
    await settle(350)
    assert.equal(document.querySelector('.issue-title')?.textContent?.trim(), '#7 SUB issue seven')
    await click(document.querySelector('.issues-list .mini-btn.primary')!)
    assert.equal(api.details.length, 1)
    assert.equal(api.details[0].repo, SUB_PATH)

    await switchRepo('')
    await act(async () => api.details[0].resolve(respond({ issue: { ...SUB_ISSUE, body: 'SUB repository body' } })))
    await settle(0)
    assert.equal(useStore.getState().drafts['bot:b1'] ?? '', '', '已過期的詳情請求不能插入任何 repo 的內容')
  } finally {
    api.restore()
  }
})
