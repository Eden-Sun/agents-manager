import test from 'node:test'
import assert from 'node:assert/strict'
import type { Bot, Host, Project } from '../api/types'
import { botOfflineHost, downFor, offlineHosts } from './hostOffline.ts'

const host = (name: string, connected: boolean, since: string | null = null) =>
  ({ name, connected, error: connected ? null : 'ssh master exited', disconnected_since: since }) as Host
const project = (id: string, h: string, label = id) => ({ id, host: h, label, path: `/p/${id}` }) as Project
const bot = (id: string, project_id: string, pending = false) => ({ id, project_id, name: id, ...(pending ? { pending } : {}) }) as Bot

test('離線主機只列有專案掛著的，帶出受影響的 bot（佔位列不算）', () => {
  const s = {
    hosts: [host('m4p', false, '2026-10-02T03:00:00.000Z'), host('spare', false), host('mini', true)],
    projects: [project('p1', 'm4p', 'hub'), project('p2', 'local'), project('p3', 'mini'), project('p4', 'm4p', 'ops')],
    bots: [bot('a', 'p1'), bot('b', 'p4'), bot('c', 'p2'), bot('d', 'p3'), bot('pending:x', 'p1', true)],
  }
  const out = offlineHosts(s)
  assert.deepEqual(out.map((h) => h.name), ['m4p'], 'spare 沒專案、mini 連著、local 不歸這裡管')
  assert.equal(out[0].since, '2026-10-02T03:00:00.000Z')
  assert.deepEqual(out[0].bots.map((b) => `${b.project}/${b.name}`), ['hub/a', 'ops/b'])
})

test('專案指到 daemon 沒回報的主機一樣算離線（同 botLamp）', () => {
  const s = { hosts: [], projects: [project('p1', 'gone')], bots: [bot('a', 'p1')] }
  assert.deepEqual(offlineHosts(s).map((h) => [h.name, h.since, h.error]), [['gone', null, null]])
  assert.equal(botOfflineHost(s, 'a'), 'gone')
})

test('botOfflineHost：本機與連著的遠端都是 null', () => {
  const s = { hosts: [host('m4p', true)], projects: [project('p1', 'm4p'), project('p2', 'local')], bots: [bot('a', 'p1'), bot('b', 'p2')] }
  assert.equal(botOfflineHost(s, 'a'), null)
  assert.equal(botOfflineHost(s, 'b'), null)
  assert.equal(botOfflineHost(s, null), null)
})

test('離線多久', () => {
  const t0 = Date.parse('2026-10-02T03:00:00.000Z')
  const at = (min: number) => downFor('2026-10-02T03:00:00.000Z', t0 + min * 60_000)
  assert.equal(at(0.5), '剛剛')
  assert.equal(at(23), '23 分鐘')
  assert.equal(at(60), '1 小時')
  assert.equal(at(95), '1 小時 35 分')
  assert.equal(at(24 * 60), '1 天')
  assert.equal(at(27 * 60 + 5), '1 天 3 小時')
  assert.equal(downFor(null, t0), null, '舊 daemon 沒這欄就不寫')
  assert.equal(downFor('garbage', t0), null)
})
