import test from 'node:test'
import assert from 'node:assert/strict'
import { ApiError } from '../api/types.ts'
import { keySyncActive, paneReadOnly, shellForbidden, shellStateUnknown } from './shellAccess.ts'

test('唯讀看 daemon 的 read_only；舊 daemon 沒這欄時退回看 port，不看 kind', () => {
  // 跑著 vim 的 shell 被掃描分成 service，但沒有 port：要打得進去，不然人出不來。
  assert.equal(paneReadOnly({ listen_ports: [] }), false)
  assert.equal(paneReadOnly({ listen_ports: [3010] }), true)
  assert.equal(paneReadOnly({ read_only: false, listen_ports: [3010] }), false, 'daemon 說了算')
  assert.equal(paneReadOnly({ read_only: true, listen_ports: [] }), true)
})

test('只有 daemon 的兩種 403 算「不給打字」，並帶出它的說明', () => {
  const ro = new ApiError(403, { error: 'read_only_pane', message: '這顆 pane 開著 port，只能看' }, 'x')
  assert.equal(shellForbidden(ro), '這顆 pane 開著 port，只能看')
  assert.match(shellForbidden(new ApiError(403, { error: 'agent_pane' }, 'x')) ?? '', /agent/)
  assert.equal(shellForbidden(new ApiError(403, { error: 'forbidden' }, 'x')), null)
  assert.equal(shellForbidden(new ApiError(502, { error: 'read_only_pane' }, 'x')), null)
  assert.equal(shellForbidden(new Error('boom')), null)
})

test('唯讀時 localStorage 記著的鍵盤同步不算數', () => {
  assert.equal(keySyncActive(true, true), false)
  assert.equal(keySyncActive(true, false), true)
  assert.equal(keySyncActive(false, false), false)
})

test('讀不到 pane 狀態的 409：顯示 daemon 的說明、不當成唯讀', () => {
  const unknown = new ApiError(409, { error: 'conflict', reason: 'pane_state_unknown', message: '請稍後再試' } as never, 'conflict')
  assert.equal(shellStateUnknown(unknown), '請稍後再試')
  assert.equal(shellForbidden(unknown), null, '不能拿去鎖面板')
  assert.equal(shellStateUnknown(new ApiError(409, { error: 'conflict', reason: 'pane_state_unknown' } as never, 'conflict'))?.includes('稍後再試'), true)
  assert.equal(shellStateUnknown(new ApiError(409, { error: 'conflict', reason: 'service_pane' } as never, 'conflict')), null)
})
