import test from 'node:test'
import assert from 'node:assert/strict'
import { routeDaemon } from './storeEnv.harness.ts'
import { useStore } from './store.ts'

// 網址 ↔ store 的競態（#1808）：shell 深連結的 `fetchHostShells` 還在等的時候使用者點了別的 bot，
// 舊的回應不可以把選取改回 shell，也不可以在失敗時把畫面送回首頁。
// 真的走正式的 `startRouteSync`／store／api transport，只換掉 location、history、popstate 與 daemon 的回應。

const g = globalThis as unknown as Record<string, unknown>
const page = { path: '/' }
const popListeners: Array<() => void> = []

type Deferred = { resolve: (v: unknown) => void; reject: (e: unknown) => void; promise: Promise<unknown> }
let shellReplies: Deferred[] = []

function deferred(): Deferred {
  let resolve!: (v: unknown) => void
  let reject!: (e: unknown) => void
  const promise = new Promise<unknown>((a, b) => {
    resolve = a
    reject = b
  })
  return { resolve, reject, promise }
}

const flush = async () => {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0))
}

/** 模擬上一頁／下一頁或深連結：網址換了，發 popstate。 */
function navigate(path: string) {
  page.path = path
  for (const fn of popListeners) fn()
}

function reset() {
  shellReplies = []
  useStore.setState({ selectedBotId: null, selectedProjectId: null, shellView: null, settingsBotId: null })
}

/**
 * 開機：每次拿一份新的 `routeSync` 模組實例（`?boot=` 讓 bun 當成另一個模組），store 仍是同一個。
 * 網址先設成要測的深連結，選取照 localStorage 的樣子先放進去，`ready` 之前不套用。
 */
let bootCount = 0
async function bootAt(path: string, bots: Array<{ id: string }>) {
  popListeners.length = 0
  shellReplies = []
  // 上一個實例的訂閱還活著：先清選取（它只會把網址寫回首頁），再設這次的網址。
  useStore.setState({ selectedBotId: null, selectedProjectId: null, shellView: null, settingsBotId: null, bots: bots as never, projects: [], ready: false })
  page.path = path
  const mod = (await import('./routeSync.ts?boot=' + ++bootCount)) as typeof import('./routeSync.ts')
  mod.startRouteSync()
  useStore.setState({ ready: true })
}

test.before(() => {
  g.location = { get pathname() { return page.path }, search: '', protocol: 'http:', host: '127.0.0.1:7788' }
  g.history = {
    state: null,
    replaceState: (_st: unknown, _t: string, url: string) => {
      page.path = url
    },
    pushState: (_st: unknown, _t: string, url: string) => {
      page.path = url
    },
  }
  g.window = {
    addEventListener: (type: string, fn: () => void) => {
      if (type === 'popstate') popListeners.push(fn)
    },
    removeEventListener: () => {},
  }
  g.document = { ...(g.document as object), querySelectorAll: () => [] }
  routeDaemon((req) => {
    if (req.method === 'GET' && req.path.endsWith('/shells')) {
      const d = deferred()
      shellReplies.push(d)
      return d.promise.then((body) => new Response(JSON.stringify(body), { status: 200 }))
    }
    return new Response('{}', { status: 200 })
  })
})

// 開機深連結是使用者明確打開的網址：localStorage 還原（非使用者動作寫進來的選取）不蓋掉它。
test('開機帶 shell 深連結，shell 還沒回來時選取被還原改掉：最後顯示那個 shell，網址仍是 shell', async () => {
  await bootAt('/hosts/remote/shells/p1', [{ id: 'b' }, { id: 'c' }])
  await flush()
  assert.equal(shellReplies.length, 1)

  useStore.setState({ selectedBotId: 'b' })
  shellReplies[0].resolve({ shells: [{ pane_id: 'p1', cwd: '/w' }] })
  await flush()

  const s = useStore.getState()
  assert.equal(s.shellView?.paneId, 'p1')
  assert.equal(page.path, '/hosts/remote/shells/p1')
})

test('開機帶 shell 深連結，shell 還沒回來時使用者點了別的 bot：使用者的選擇贏，網址一致', async () => {
  await bootAt('/hosts/remote/shells/p1', [{ id: 'b' }, { id: 'c' }])
  await flush()
  assert.equal(shellReplies.length, 1)

  useStore.getState().selectBot('c')
  shellReplies[0].resolve({ shells: [{ pane_id: 'p1', cwd: '/w' }] })
  await flush()

  const s = useStore.getState()
  assert.equal(s.selectedBotId, 'c')
  assert.equal(s.shellView, null)
  assert.equal(page.path, '/bots/c')
})

test('開機帶 shell 深連結，shell 正常回來：顯示那個 shell，網址不變', async () => {
  await bootAt('/hosts/remote/shells/p1', [{ id: 'b' }])
  await flush()
  shellReplies[0].resolve({ shells: [{ pane_id: 'p1', cwd: '/w' }] })
  await flush()

  const s = useStore.getState()
  assert.equal(s.shellView?.paneId, 'p1')
  assert.equal(page.path, '/hosts/remote/shells/p1')
})

test('shell 還沒回來時使用者改選別的 bot：舊結果不改選取，網址維持新的 bot', async () => {
  reset()
  navigate('/hosts/remote/shells/p1')
  await flush()
  assert.equal(shellReplies.length, 1)

  useStore.getState().selectBot('b')
  shellReplies[0].resolve({ shells: [{ pane_id: 'p1', cwd: '/w' }] })
  await flush()

  const s = useStore.getState()
  assert.equal(s.selectedBotId, 'b')
  assert.equal(s.shellView, null)
  assert.equal(page.path, '/bots/b')
})

test('shell 還沒回來時使用者改選 bot，之後 shell 失敗：不回首頁蓋掉新的選取', async () => {
  reset()
  navigate('/hosts/remote/shells/p2')
  await flush()
  useStore.getState().selectBot('b')

  shellReplies[0].reject(new Error('daemon down'))
  await flush()

  assert.equal(useStore.getState().selectedBotId, 'b')
  assert.equal(page.path, '/bots/b')
})

test('較新的 shell 導覽還沒回來時，先前那條 shell 晚到不改畫面', async () => {
  reset()
  navigate('/hosts/remote/shells/p1')
  await flush()
  navigate('/hosts/remote/shells/p2')
  await flush()
  assert.equal(shellReplies.length, 2)

  shellReplies[1].resolve({ shells: [{ pane_id: 'p2', cwd: '/w' }] })
  await flush()
  shellReplies[0].resolve({ shells: [{ pane_id: 'p1', cwd: '/w' }] })
  await flush()

  const s = useStore.getState()
  assert.equal(s.shellView?.paneId, 'p2')
  assert.equal(page.path, '/hosts/remote/shells/p2')
})
