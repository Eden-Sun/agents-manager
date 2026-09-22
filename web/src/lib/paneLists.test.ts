import test from 'node:test'
import assert from 'node:assert/strict'
import type { ProjectPane } from '../api/index.ts'
import { groupByProject, unownedRows, withPane, withoutPane } from './paneLists.ts'

const pane = (pane_id: string, over: Partial<ProjectPane> = {}): ProjectPane => ({
  pane_id,
  host: 'local',
  workspace_id: 'w1',
  tab_id: 'w1:t1',
  cwd: '/p',
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
  ...over,
})

test('沒歸屬的 pane 不掛在任何專案底下（以前每個專案底下各出現一次）', () => {
  const scratch = pane('w1:pS', { project_id: null, owned_by: 'none' })
  const grouped = groupByProject([pane('w1:p1'), pane('w1:p2', { project_id: 'p2' }), scratch])
  assert.deepEqual(Object.keys(grouped).sort(), ['p1', 'p2'])
  assert.ok(!Object.values(grouped).flat().includes(scratch))
})

test('側欄底部：daemon 有標 scratch 就排第一、其他標「多出來的」；舊 daemon 沒這欄就全部列、不標', () => {
  const extra = pane('w1:pX', { project_id: null, scratch: false })
  const scratch = pane('w1:pS', { project_id: null, scratch: true })
  assert.deepEqual(
    unownedRows([extra, scratch]).map((r) => [r.pane.pane_id, r.tag]),
    [['w1:pS', 'scratch'], ['w1:pX', 'extra']],
  )
  const old = [pane('w1:pA', { project_id: null }), pane('w1:pB', { project_id: null })]
  assert.deepEqual(unownedRows(old).map((r) => r.tag), [null, null])
})

test('關掉一顆：兩份清單一起拿掉，空掉的專案整組消失', () => {
  const lists = { sidePanes: { p1: [pane('w1:p1')], p2: [pane('w1:p2', { project_id: 'p2' })] }, unownedPanes: [pane('w1:p1', { host: 'm4p' })] }
  const next = withoutPane(lists, 'local', 'w1:p1')
  assert.equal(next.sidePanes.p1, undefined)
  assert.equal(next.sidePanes.p2.length, 1)
  assert.equal(next.unownedPanes.length, 1, 'pane id 只在同一台主機內唯一')
})

test('daemon 附上的最新那一列換掉舊的', () => {
  const lists = { sidePanes: { p1: [pane('w1:p1')] }, unownedPanes: [] }
  const fresh = pane('w1:p1', { kind: 'service', listen_ports: [3010] })
  assert.equal(withPane(lists, fresh).sidePanes.p1[0].kind, 'service')
})

test('project_id 從一個專案改到另一個：舊組移除，新組接在原有順序後', () => {
  const lists = {
    sidePanes: {
      p1: [pane('p1-before'), pane('w1:moving'), pane('p1-after')],
      p2: [pane('p2-first', { project_id: 'p2' }), pane('p2-last', { project_id: 'p2' })],
    },
    unownedPanes: [],
  }
  const fresh = pane('w1:moving', { project_id: 'p2', kind: 'service' })

  const next = withPane(lists, fresh)
  assert.deepEqual(next.sidePanes.p1.map((p) => p.pane_id), ['p1-before', 'p1-after'])
  assert.deepEqual(next.sidePanes.p2.map((p) => p.pane_id), ['p2-first', 'p2-last', 'w1:moving'])
  assert.equal(next.sidePanes.p2.at(-1), fresh)
})

test('同 host+pane_id 在兩份清單或多個專案重複時，只留在 fresh 指定分組', () => {
  const fresh = pane('w1:moving', { project_id: 'p2', kind: 'service' })
  const remoteCopy = pane('w1:moving', { host: 'remote', project_id: 'p1' })
  const lists = {
    sidePanes: {
      p1: [pane('p1-before'), pane('w1:moving'), remoteCopy, pane('p1-after')],
      p2: [pane('p2-before', { project_id: 'p2' }), pane('w1:moving', { project_id: 'p2' }), pane('p2-after', { project_id: 'p2' })],
    },
    unownedPanes: [pane('w1:moving', { project_id: null, owned_by: 'none' }), pane('unowned-after', { project_id: null, owned_by: 'none' })],
  }

  const next = withPane(lists, fresh)
  assert.deepEqual(next.sidePanes.p1.map((p) => p.pane_id), ['p1-before', 'w1:moving', 'p1-after'])
  assert.equal(next.sidePanes.p1[1], remoteCopy)
  assert.deepEqual(next.sidePanes.p2.map((p) => p.pane_id), ['p2-before', 'w1:moving', 'p2-after'])
  assert.equal(next.sidePanes.p2[1], fresh, '保留目的分組中原有的位置')
  assert.deepEqual(next.unownedPanes.map((p) => p.pane_id), ['unowned-after'])
  const localCopies = [...Object.values(next.sidePanes).flat(), ...next.unownedPanes]
    .filter((p) => p.host === 'local' && p.pane_id === 'w1:moving')
  assert.deepEqual(localCopies, [fresh])
})

test('project_id 改成 null：從專案組移到未歸屬清單並保留目標位置', () => {
  const lists = {
    sidePanes: { p1: [pane('p1-before'), pane('w1:moving'), pane('p1-after')] },
    unownedPanes: [
      pane('unowned-before', { project_id: null, owned_by: 'none' }),
      pane('w1:moving', { project_id: null, owned_by: 'none' }),
      pane('unowned-after', { project_id: null, owned_by: 'none' }),
    ],
  }
  const fresh = pane('w1:moving', { project_id: null, owned_by: 'none', scratch: true })

  const next = withPane(lists, fresh)
  assert.deepEqual(next.sidePanes.p1.map((p) => p.pane_id), ['p1-before', 'p1-after'])
  assert.deepEqual(next.unownedPanes.map((p) => p.pane_id), ['unowned-before', 'w1:moving', 'unowned-after'])
  assert.equal(next.unownedPanes[1], fresh)
})
