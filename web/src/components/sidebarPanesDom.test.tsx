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
