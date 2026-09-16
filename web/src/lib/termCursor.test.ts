import test from 'node:test'
import assert from 'node:assert/strict'
import { splitAtCursor } from './termCursor.ts'

test('游標落在最後一行有字的行尾，提示字元後補一格', () => {
  assert.deepEqual(splitAtCursor('ls\nfoo bar\nm4p@m4p agents-manager %   \n\n   \n'), {
    before: 'ls\nfoo bar\nm4p@m4p agents-manager %',
    gap: ' ',
    after: '\n\n   \n',
  })
  assert.deepEqual(splitAtCursor('m4p@m4p ~ % exit'), { before: 'm4p@m4p ~ % exit', gap: '', after: '' })
  assert.deepEqual(splitAtCursor('\n\n'), { before: '', gap: '', after: '\n\n' })
})
