import test from 'node:test'
import assert from 'node:assert/strict'
import type { ProjectPane } from '../api/index.ts'
import { paneHint, paneLabel } from './panes.ts'

const pane = (over: Partial<ProjectPane> = {}): ProjectPane => ({
  pane_id: 'w1:p7',
  host: 'local',
  workspace_id: 'w1',
  tab_id: 'w1:t1',
  cwd: '/Users/me/project/agents-manager',
  kind: 'shell',
  owned_by: 'bot',
  owner_bot_id: null,
  project_id: 'p1',
  purpose: null,
  foreground: null,
  listen_ports: [],
  last_output_at: '2026-09-16T08:00:00Z',
  first_seen: '2026-09-16T07:00:00Z',
  last_seen: '2026-09-16T08:00:00Z',
  gc_optin: false,
  ...over,
})

test('預設使用前景程式的執行檔名，沒有指令才用用途', () => {
  assert.equal(paneLabel(pane({ purpose: 'build' })), 'build')
  assert.equal(paneLabel(pane({ purpose: '  ', foreground: '/opt/homebrew/bin/node next dev' })), 'node')
})

test('什麼都沒有時退到 cwd 的最後一段，再退到 pane id——絕不會是空字串', () => {
  assert.equal(paneLabel(pane()), 'agents-manager')
  assert.equal(paneLabel(pane({ cwd: '/Users/me/project/am/' })), 'am')
  assert.equal(paneLabel(pane({ cwd: null })), 'w1:p7')
  assert.equal(paneLabel(pane({ cwd: '/' })), 'w1:p7')
})

test('提示講得出在哪、開了什麼 port、是不是自己開的', () => {
  assert.equal(paneHint(pane()), '/Users/me/project/agents-manager')
  assert.match(paneHint(pane({ listen_ports: [3010, 5432] })), /listen 3010、5432/)
  assert.match(paneHint(pane({ owned_by: 'user' })), /你自己開的/)
})

test('別名優先，預設標題帶 listen port', () => {
  assert.equal(paneLabel(pane({ purpose: 'dev-server', foreground: 'bun run dev', listen_ports: [3200] })), 'bun :3200')
  assert.equal(paneLabel(pane({ alias: 'example-ops', foreground: 'bun run dev', listen_ports: [3200] })), 'example-ops')
})
