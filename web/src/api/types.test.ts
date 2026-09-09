import test from 'node:test'
import assert from 'node:assert/strict'
import { teamDisplayName } from './types.ts'

test('teamDisplayName keeps the issue tag only when short names collide', () => {
  const one = ['tabc123-pm', 'tabc123-i1-dev-1', 'tabc123-rev']
  assert.equal(teamDisplayName('tabc123-i1-dev-1', one), 'dev-1')
  const many = ['tabc123-pm', 'tabc123-i1-dev-1', 'tabc123-i2-dev-1', 'tabc123-i3-dev-1', 'tabc123-i1-dev-2']
  assert.equal(teamDisplayName('tabc123-i2-dev-1', many), 'i2-dev-1')
  assert.equal(teamDisplayName('tabc123-i1-dev-2', many), 'dev-2')
  assert.equal(teamDisplayName('tabc123-pm', many), 'pm')
})
