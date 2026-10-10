import test from 'node:test'
import assert from 'node:assert/strict'
import { bumpEventRevs } from './eventRevs.ts'

test('重連／resync：總管 rev 與每顆 bot 的分享 rev 都加一，輸入的物件不被改動', () => {
  const shareRev = { b1: 3 }
  const out = bumpEventRevs({ supervisorRev: 2, shareRev, bots: [{ id: 'b1' }, { id: 'b2' }] })
  assert.deepEqual(out, { supervisorRev: 3, shareRev: { b1: 4, b2: 1 } })
  assert.deepEqual(shareRev, { b1: 3 })
})
