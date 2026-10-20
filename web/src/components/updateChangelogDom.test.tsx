/**
 * 更新框的翻譯（#1870）：原文一直在，翻譯是另外一層。翻譯鈕跟回應的 lang。
 * 輪詢上限用 `translationPollAction` 斷言，不真的等 40 次。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { TRANSLATION_POLL_MAX, translationPollAction } from '../api/changelog'
import { UpdateChangelog } from './UpdateChangelog'
import { click, fakeApi, mount, setupDom, settle, teardownDom, unmountAll, until, type FakeRequest } from '../testing/domHarness'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const CHANGELOG = {
  kind: 'claude',
  host: 'local',
  installed_version: '2.1.5',
  from_version: '2.1.3',
  found: true,
  sections: [
    { version: '2.1.5', body: '- Added widgets for the update window' },
    { version: '2.1.4', body: '- Fixed the status line' },
  ],
  source_url: 'https://example.test/CHANGELOG.md',
  error: null,
}

function pathOf(raw: string): { pathname: string; q: URLSearchParams } {
  const u = new URL(raw, 'http://local')
  return { pathname: u.pathname, q: u.searchParams }
}

test('輪詢第 40 次仍查，第 41 次停止', () => {
  assert.equal(translationPollAction(TRANSLATION_POLL_MAX), 'poll')
  assert.equal(translationPollAction(TRANSLATION_POLL_MAX + 1), 'stop')
})

test('未設定不畫翻譯鈕、也不畫錯誤', async () => {
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation') return { enabled: false, status: 'unconfigured', error: null, summary: [], sections: [] }
    return {}
  })
  const root = await mount(
    <div>
      <UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />
      <button type="button">請 AGM 解析</button>
      <button type="button">重啟套用</button>
    </div>,
  )
  await until(() => root.textContent?.includes('Added widgets') === true, '原文')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
  assert.equal(root.textContent?.includes('翻譯沒有完成'), false)
  assert.ok(root.querySelector('a.update-changelog-src'))
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '請 AGM 解析')?.disabled, false)
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '重啟套用')?.disabled, false)
})

function absentLang(lang: string | null) {
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation') {
      return { enabled: true, status: 'absent', lang, error: null, summary: [], sections: [] }
    }
    return {}
  })
}

test('ja 的翻譯鈕寫日文，不寫繁中', async () => {
  absentLang('ja')
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成日文'), '日文')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
  assert.equal(root.textContent?.includes('繁中摘要'), false)
})

test('zh-Hant 的翻譯鈕寫繁中', async () => {
  absentLang('zh-Hant')
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), '繁中')
})

test('簡中的翻譯鈕跟 lang', async () => {
  absentLang('zh-Hans')
  const hans = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...hans.querySelectorAll('button')].some((b) => b.textContent === '翻譯成簡中'), '簡中')
})

test('英文翻譯鈕', async () => {
  absentLang('en')
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成英文'), '英文')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
})

test('韓文翻譯鈕', async () => {
  absentLang('ko')
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成韓文'), '韓文')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
})

test('沒有 lang 或不認得時翻譯鈕不寫目標語言', async () => {
  absentLang(null)
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯'), '中立')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
})

test('不認得的 lang 不猜成繁中', async () => {
  absentLang('fr')
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯'), '不認得')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
})

test('absent 時按一次才 POST', async () => {
  const posts: string[] = []
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation' && req.method === 'GET') return { enabled: true, status: 'absent', error: null }
    if (pathname === '/api/changelog/translation' && req.method === 'POST') {
      posts.push(req.method)
      return { enabled: true, status: 'completed', summary: ['第一點', '第二點', '第三點'], sections: [{ version: '2.1.5', body: '繁中 2.1.5' }], coverage: 'full', error: null }
    }
    return {}
  })
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => root.querySelector('button')?.textContent === '翻譯', '翻譯鈕')
  await click(root.querySelector('button')!)
  await until(() => root.textContent?.includes('第一點') === true, '摘要')
  assert.equal(posts.length, 1)
  await settle(50)
  assert.equal(posts.length, 1)
  assert.ok(root.querySelector('a.update-changelog-src'))
})

test('失敗時原文還在，再試一次只 POST 一次；未來的 retry_after 按了也不送', async () => {
  const posts: FakeRequest[] = []
  const later = new Date(Date.now() + 60_000).toISOString()
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname !== '/api/changelog/translation') return {}
    if (req.method === 'POST') posts.push(req)
    return { enabled: true, status: 'failed', error: 'timeout', error_kind: 'timeout', retry_after: later, summary: [], sections: [], attempts: [{ usage: null }] }
  })
  const root = await mount(
    <div>
      <UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />
      <button type="button">請 AGM 解析</button>
      <button type="button">重啟套用</button>
    </div>,
  )
  await until(() => root.textContent?.includes('翻譯沒有完成') === true, '失敗狀態')
  assert.ok(root.textContent?.includes('Added widgets'))
  const retry = [...root.querySelectorAll('button')].find((b) => b.textContent === '再試一次')
  assert.equal(retry?.disabled, true)
  if (retry) await click(retry)
  await settle(20)
  assert.equal(posts.length, 0)
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '請 AGM 解析')?.disabled, false)
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '重啟套用')?.disabled, false)
})

test('retry_after 過了之後再試一次才 POST', async () => {
  const posts: string[] = []
  const soon = new Date(Date.now() + 400).toISOString()
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation' && req.method === 'POST') {
      posts.push('POST')
      return { enabled: true, status: 'completed', summary: ['補翻完成', '第二點', '第三點'], sections: [{ version: '2.1.5', body: '繁中 2.1.5' }], coverage: 'full', error: null }
    }
    if (pathname === '/api/changelog/translation') {
      return { enabled: true, status: 'failed', error: 'timeout', retry_after: soon, summary: [], sections: [] }
    }
    return {}
  })
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => root.textContent?.includes('再試一次') === true, '再試')
  const button = () => [...root.querySelectorAll('button')].find((b) => b.textContent === '再試一次')
  assert.equal(button()?.disabled, true)
  await until(() => button()?.disabled === false, '退避結束', 3000)
  await click(button()!)
  await until(() => root.textContent?.includes('補翻完成') === true, '完成')
  assert.equal(posts.length, 1)
})

test('讀不到 retry_after 時再試一次維持停用', async () => {
  const posts: string[] = []
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (req.method === 'POST') posts.push('POST')
    if (pathname === '/api/changelog/translation') return { enabled: true, status: 'failed', error: 'http', retry_after: 'not-a-date', summary: [], sections: [] }
    return {}
  })
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => root.textContent?.includes('再試一次') === true, '再試')
  const retry = [...root.querySelectorAll('button')].find((b) => b.textContent === '再試一次')
  assert.equal(retry?.disabled, true)
  if (retry) await click(retry)
  await settle(600)
  assert.equal(retry?.disabled, true)
  assert.equal(posts.length, 0)
})

test('換目標版本後舊摘要立刻消失', async () => {
  function Probe() {
    const [to, setTo] = useState('2.1.5')
    return (
      <div>
        <button type="button" onClick={() => setTo('2.1.4')}>換版本</button>
        <UpdateChangelog kind="claude" host="local" from="2.1.3" to={to} />
      </div>
    )
  }
  fakeApi((req) => {
    const { pathname, q } = pathOf(req.path)
    if (pathname === '/api/changelog') {
      const to = q.get('to')
      return { ...CHANGELOG, installed_version: to, sections: CHANGELOG.sections.filter((s) => s.version === to || to === '2.1.5') }
    }
    if (pathname === '/api/changelog/translation') {
      const to = q.get('to')
      if (to === '2.1.5') return { enabled: true, status: 'completed', summary: ['舊摘要不該留下', '第二點', '第三點'], sections: [{ version: '2.1.5', body: '繁中舊版' }], coverage: 'full', error: null }
      return { enabled: true, status: 'absent', error: null, summary: [], sections: [] }
    }
    return {}
  })
  const root = await mount(<Probe />)
  await until(() => root.textContent?.includes('舊摘要不該留下') === true, '舊摘要')
  await click([...root.querySelectorAll('button')].find((b) => b.textContent === '換版本')!)
  assert.equal(root.textContent?.includes('舊摘要不該留下'), false)
  await until(() => [...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯'), '新版本還沒翻')
  assert.equal([...root.querySelectorAll('button')].some((b) => b.textContent === '翻譯成繁中'), false)
  assert.equal(root.textContent?.includes('舊摘要不該留下'), false)
})

test('進行中只輪詢 GET，不 POST', async () => {
  const methods: string[] = []
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation') {
      methods.push(req.method)
      return { enabled: true, status: 'in_flight', error: null, summary: [], sections: [], attempts: [] }
    }
    return {}
  })
  const root = await mount(<UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />)
  await until(() => root.textContent?.includes('正在翻譯') === true, '進行中')
  const before = methods.filter((m) => m === 'GET').length
  await until(() => methods.filter((m) => m === 'GET').length > before, '下一輪 GET', 4000)
  assert.equal(methods.filter((m) => m === 'POST').length, 0)
  assert.ok(root.querySelector('a.update-changelog-src'))
})

test('翻譯連不上時留一行，原文與旁邊的按鈕都還在', async () => {
  fakeApi((req) => {
    const { pathname } = pathOf(req.path)
    if (pathname === '/api/changelog') return CHANGELOG
    if (pathname === '/api/changelog/translation') throw new Error('offline')
    return {}
  })
  const root = await mount(
    <div>
      <UpdateChangelog kind="claude" host="local" from="2.1.3" to="2.1.5" />
      <button type="button">請 AGM 解析</button>
      <button type="button">重啟套用</button>
    </div>,
  )
  await until(() => root.textContent?.includes('翻譯狀態暫時讀不到') === true, '連不上')
  assert.ok(root.textContent?.includes('Added widgets'))
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '請 AGM 解析')?.disabled, false)
  assert.equal([...root.querySelectorAll('button')].find((b) => b.textContent === '重啟套用')?.disabled, false)
})
