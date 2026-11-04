import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import type { ProjectPane } from '../api'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { SidebarPanes } from './SidebarPanes'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const pane: ProjectPane = {
  pane_id: 'w1:p1',
  host: 'local',
  workspace_id: null,
  tab_id: null,
  cwd: null,
  kind: 'shell',
  owned_by: 'user',
  owner_bot_id: null,
  project_id: 'p1',
  purpose: null,
  foreground: null,
  listen_ports: [],
  last_output_at: '',
  first_seen: '',
  last_seen: '',
  gc_optin: false,
}

test('pane 列是按鈕，不是 listitem', async () => {
  useStore.setState({ sidePanes: { p1: [pane] }, shellView: null } as never)
  await mount(<SidebarPanes projectId="p1" />)
  const b = document.querySelector('.side-pane')!
  assert.equal(b.tagName, 'BUTTON')
  assert.equal(b.hasAttribute('role'), false)
  assert.equal(b.closest('.side-panes')!.getAttribute('role'), 'group')
  assert.equal(b.hasAttribute('aria-current'), false)
})

test('正開著的那一顆標 aria-current', async () => {
  useStore.setState({ sidePanes: { p1: [pane] }, shellView: { host: 'local', paneId: 'w1:p1', cwd: null } } as never)
  await mount(<SidebarPanes projectId="p1" />)
  const b = document.querySelector('.side-pane')!
  assert.equal(b.getAttribute('aria-current'), 'true')
})

test('shell 列有「複製」：同一台、同一個目錄新開一個；服務 pane、沒有 cwd 的沒有', async () => {
  const calls: unknown[][] = []
  const shellPane = { ...pane, cwd: '/home/u/proj' }
  const svc = { ...pane, pane_id: 'w1:p2', cwd: '/home/u/proj', kind: 'service' as const, listen_ports: [3000] }
  const bare = { ...pane, pane_id: 'w1:p3' }
  useStore.setState({
    sidePanes: { p1: [shellPane, svc, bare] },
    shellView: null,
    busy: {},
    openHostShell: async (...a: unknown[]) => (calls.push(a), true),
    refreshPanes: async () => {},
  } as never)
  await mount(<SidebarPanes projectId="p1" />)
  const clones = document.querySelectorAll<HTMLButtonElement>('.side-pane-clone')
  assert.equal(clones.length, 1)
  clones[0].click()
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(calls, [['local', '/home/u/proj']])
})
