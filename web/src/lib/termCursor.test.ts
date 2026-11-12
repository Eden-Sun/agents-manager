import test from 'node:test'
import assert from 'node:assert/strict'
import { splitAtCursor } from './termCursor.ts'

test('游標落在最後一行有字的行尾，提示字元後補一格', () => {
  assert.deepEqual(splitAtCursor('ls\nfoo bar\nmac_builder@mac_builder agents-manager %   \n\n   \n'), {
    before: 'ls\nfoo bar\nmac_builder@mac_builder agents-manager %',
    gap: ' ',
    after: '\n\n   \n',
  })
  assert.deepEqual(splitAtCursor('mac_builder@mac_builder ~ % exit'), { before: 'mac_builder@mac_builder ~ % exit', gap: '', after: '' })
  assert.deepEqual(splitAtCursor('\n\n'), { before: '', gap: '', after: '\n\n' })
})
