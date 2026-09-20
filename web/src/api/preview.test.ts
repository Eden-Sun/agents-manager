import assert from 'node:assert/strict'
import { test } from 'node:test'
import { PREVIEW_OFF, groupOthers, kindLabel, toPreview, toPreviewEvent } from './preview'

test('toPreview: 沒開過／壞資料都是 off', () => {
  assert.deepEqual(toPreview({ status: 'off' }), PREVIEW_OFF)
  assert.deepEqual(toPreview(null), PREVIEW_OFF)
  assert.equal(toPreview({ status: 'weird', port: 5180 }).status, 'off')
})

test('toPreviewEvent: running 沿用 dir，離開 failed 清錯誤，off 清 port', () => {
  const failed = { ...PREVIEW_OFF, status: 'failed' as const, port: 5180, dir: '/x/web', error: 'boom' }
  const starting = toPreviewEvent({ bot_id: 'b', status: 'starting', port: 5181 }, failed)
  assert.equal(starting.error, null)
  assert.equal(starting.dir, '/x/web')
  assert.equal(starting.port, 5181)
  const off = toPreviewEvent({ bot_id: 'b', status: 'off', port: null }, starting)
  assert.equal(off.port, null)
  assert.equal(off.status, 'off')
})

test('previewApiMissing: 404／405 是舊 daemon，其他錯誤不是', async () => {
  const { ApiError } = await import('./types')
  const { previewApiMissing } = await import('./preview')
  assert.equal(previewApiMissing(new ApiError(405, {}, 'x')), true)
  assert.equal(previewApiMissing(new ApiError(404, {}, 'x')), true)
  assert.equal(previewApiMissing(new ApiError(409, { reason: 'no_vite_config' }, 'x')), false)
  assert.equal(previewApiMissing(new Error('x')), false)
})

test('toPreview: v2 的 source／candidates／others，壞項目丟掉', () => {
  const p = toPreview({
    status: 'running',
    port: 5173,
    source: 'attached',
    pid: 4242,
    candidates: ['/a/web', 3, ''],
    others: [{ port: 3001, dir: '/b/apps/web', pid: 9 }, { port: 'x', dir: '/c' }, null],
  })
  assert.equal(p.source, 'attached')
  assert.equal(p.pid, 4242)
  assert.deepEqual(p.candidates, [{ dir: '/a/web', command: null }])
  assert.deepEqual(p.others, [{ port: 3001, dir: '/b/apps/web', pid: 9, relation: 'same_repo', kind: 'vite', repo: null }])
  assert.deepEqual(toPreview({ status: 'off' }).others, [])
  assert.equal(toPreview({ status: 'running', source: 'weird' }).source, null)
})

test('v3 others：relation 分組固定順序、空組不出、缺 relation 照 same_repo', () => {
  const p = toPreview({
    status: 'off',
    others: [
      { port: 3001, dir: '/o/apps/web', relation: 'other', repo: 'hermes' },
      { port: 5173, dir: '/r/web', relation: 'same_repo' },
      { port: 5241, dir: '/me/web', relation: 'same_dir' },
      { port: 5300, dir: '/old/web' },
    ],
  })
  const g = groupOthers(p.others)
  assert.deepEqual(g.map((x) => x.relation), ['same_dir', 'same_repo', 'other'])
  assert.deepEqual(g[1].items.map((o) => o.port), [5173, 5300])
  assert.equal(g[2].items[0].repo, 'hermes')
  assert.deepEqual(groupOthers([]), [])
})

test('v4：candidates 可帶指令、others 帶 kind、kind 標籤', () => {
  const p = toPreview({
    status: 'off',
    command: 'bun run dev',
    candidates: [{ dir: '/a', command: 'bun run dev' }, { dir: '/b' }, '/c', { command: 'x' }],
    others: [{ port: 3200, dir: '/w', kind: 'next', relation: 'same_dir' }],
  })
  assert.deepEqual(p.candidates, [{ dir: '/a', command: 'bun run dev' }, { dir: '/b', command: null }, { dir: '/c', command: null }])
  assert.equal(p.command, 'bun run dev')
  assert.equal(p.others[0].kind, 'next')
  assert.equal(kindLabel('next'), 'Next.js')
  assert.equal(kindLabel('unknown'), '其他')
  assert.equal(kindLabel('foo'), 'Foo')
})
