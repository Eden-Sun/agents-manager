import test from 'node:test'
import assert from 'node:assert/strict'
import { portProblem, sessionProblem, sshTargetProblem } from './hostForm.ts'

test('ssh 目標：跟 daemon 同一條規則（開頭 -、@ 後面以 - 開頭、空白與控制字元都不行）', () => {
  for (const ok of ['m4p', 'me@10.0.0.2', 'user@host.example', 'ssh-alias_1']) assert.equal(sshTargetProblem(ok), null, ok)
  for (const bad of ['', '-oProxyCommand=true', 'me@-oProxyCommand=x', 'a b', 'a\tb', 'a\nb']) assert.ok(sshTargetProblem(bad), JSON.stringify(bad))
})

test('herdr_session：1–64 字、英數開頭、只有英數 . _ -', () => {
  for (const ok of ['agents-manager', 'a', 'S1.x_y']) assert.equal(sessionProblem(ok), null, ok)
  for (const bad of ['', '-x', '_x', 'a b', 'a/b', 'x'.repeat(65)]) assert.ok(sessionProblem(bad), JSON.stringify(bad))
})

test('port：1–65535 的整數；打錯不是「變成預設值」', () => {
  assert.deepEqual(portProblem('22'), null)
  assert.equal(portProblem('65535'), null)
  for (const bad of ['', '0', '65536', '2222x', 'abc', '-1', '1.5', ' ']) assert.ok(portProblem(bad), JSON.stringify(bad))
})
