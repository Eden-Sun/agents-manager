import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })
const flush = async () => {
  for (let i = 0; i < 12; i += 1) await Promise.resolve()
}

function installBrowser() {
  const location = { pathname: '/', search: '' }
  const calls: string[] = []
  const listeners = new Map<string, () => void>()
  let state: unknown = null
  const write = (kind: string, nextState: unknown, url?: string | URL | null) => {
    state = nextState
    if (url != null) location.pathname = new URL(String(url), 'http://test').pathname
    calls.push(`${kind}:${location.pathname}`)
  }
  const history = {
    get state() { return state },
    pushState: (nextState: unknown, _title: string, url?: string | URL | null) => write('push', nextState, url),
    replaceState: (nextState: unknown, _title: string, url?: string | URL | null) => write('replace', nextState, url),
    back() {},
  }
  const window = {
    addEventListener: (type: string, listener: () => void) => listeners.set(type, listener),
  }
  Object.assign(globalThis, { window, history, location })
  return {
    calls,
    location,
    pop(path: string) {
      location.pathname = path
      listeners.get('popstate')?.()
    },
  }
}

test('stale shell route requests cannot replace a newer popstate or bot route', async () => {
  const browser = installBrowser()
  reset()
  const { useStore } = await import('./store.ts')
  const { startRouteSync } = await import('./routeSync.ts')
  useStore.setState({
    ready: false,
    bots: ['b0', 'b1', 'b2'].map((id) => ({
      id, name: id, project_id: 'p1', kind: 'claude', identity: null, managed_by: 'user', parent_bot_id: null,
    }) as Bot),
    projects: [{ id: 'p1', label: 'Project', path: '/p1', host: 'local' } as Project],
    loadedBots: { b0: true, b1: true, b2: true },
    selectedBotId: 'b0',
    selectedProjectId: null,
    settingsBotId: null,
    shellView: null,
    rightTab: 'chat',
  })
  startRouteSync()
  useStore.setState({ ready: true })
  await flush()
  assert.equal(browser.location.pathname, '/bots/b0')
  assert.deepEqual(browser.calls, ['replace:/bots/b0'], 'the initial root route still replaces history')

  const oldShells = deferred<Response>()
  const newShells = deferred<Response>()
  const oldRequested = deferred<void>()
  const newRequested = deferred<void>()
  const tracedPanes = deferred<Response>()
  const tracedRequested = deferred<void>()
  routeDaemon((request) => {
    if (request.path.includes('/hosts/old/shells')) {
      oldRequested.resolve()
      return oldShells.promise
    }
    if (request.path.includes('/hosts/new/shells')) {
      newRequested.resolve()
      return newShells.promise
    }
    if (request.path.includes('/hosts/traced/shells')) return json({ shells: [] })
    if (request.path.endsWith('/panes')) {
      tracedRequested.resolve()
      return tracedPanes.promise
    }
    return json({})
  })

  browser.pop('/hosts/old/shells/pane-old')
  await oldRequested.promise
  browser.pop('/hosts/new/shells/pane-new')
  await newRequested.promise
  oldShells.resolve(json({ shells: [{ host: 'old', pane_id: 'pane-old', cwd: '/old' }] }))
  await flush()
  assert.equal(useStore.getState().shellView, null, 'the older onPop cannot apply while the newer shell request waits')
  assert.equal(browser.location.pathname, '/hosts/new/shells/pane-new')
  newShells.resolve(json({ shells: [{ host: 'new', pane_id: 'pane-new', cwd: '/new' }] }))
  await flush()
  assert.equal(useStore.getState().shellView?.paneId, 'pane-new')

  browser.pop('/hosts/traced/shells/pane-traced')
  await tracedRequested.promise
  useStore.getState().selectBot('b2')
  await flush()
  assert.equal(browser.location.pathname, '/bots/b2', 'a direct bot navigation wins while fetchAllPanes waits')
  tracedPanes.resolve(json({ panes: [{ host: 'traced', pane_id: 'pane-traced' }] }))
  await flush()
  assert.equal(useStore.getState().selectedBotId, 'b2')
  assert.equal(useStore.getState().shellView, null)
  assert.equal(browser.location.pathname, '/bots/b2')
})
