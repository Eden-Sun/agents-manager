import assert from 'node:assert/strict'
import { test } from 'node:test'
import type { PreviewOther } from '../api/preview'
import { orderRows, shortPath, showRepo } from './previewList'

const o = (port: number, dir: string, kind = 'vite'): PreviewOther => ({ port, dir, pid: null, relation: 'other', kind, repo: null })

test('shortPath: 專案底下相對於根、根本身取最後一段、專案外最後兩段', () => {
  const root = '/Users/m/hermes-agents/projects/wt'
  assert.equal(shortPath('/Users/m/hermes-agents/projects/wt/webui/apps/web', root), 'webui/apps/web')
  assert.equal(shortPath('/Users/m/hermes-agents/projects/wt', root), 'wt')
  assert.equal(shortPath('/Users/m/project/agents-manager-main/web', root), 'agents-manager-main/web')
  assert.equal(shortPath('/Users/m/project/agents-manager-main/web', null), 'agents-manager-main/web')
  assert.equal(shortPath('/a/wtx/y', '/a/wt'), 'wtx/y')
})

test('showRepo: 跟短路徑重複就不印', () => {
  assert.equal(showRepo('wt', 'wt'), false)
  assert.equal(showRepo('agents-manager', 'agents-manager-main/web'), true)
  assert.equal(showRepo('agents-manager', 'agents-manager/web'), false)
  assert.equal(showRepo(null, 'x'), false)
})

test('orderRows: 認得出的在前照 port、unknown 另收、同目錄排在一起並標記', () => {
  const { known, unknown } = orderRows(
    [o(5556, '/r/bff', 'unknown'), o(3200, '/r/ops', 'next'), o(3000, '/r/bff', 'unknown'), o(5173, '/r/web'), o(5556 + 1, '/r/web', 'vite')],
    '/r',
  )
  assert.deepEqual(known.map((x) => x.o.port), [3200, 5173, 5557])
  assert.deepEqual(known.map((x) => x.sharedDir), [false, true, true])
  assert.deepEqual(unknown.map((x) => x.o.port), [3000, 5556])
  assert.equal(unknown.every((x) => x.sharedDir), true)
  assert.equal(known[0].short, 'ops')
})
