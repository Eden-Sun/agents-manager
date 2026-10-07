import test from 'node:test'
import assert from 'node:assert/strict'
import { newDirNameProblem } from './newDirName'

test('新資料夾的名字：空白、. / ..、含 /、控制或看不見的字元、太長都擋，並講出原因', () => {
  for (const bad of ['', '   ', '.', '..', 'a/b', '../x', '/abs', 'a\nb', 'tab\tname', 'rtl‮text', 'zw​sp', 'x'.repeat(256)]) {
    assert.ok(newDirNameProblem(bad), JSON.stringify(bad))
  }
  assert.match(newDirNameProblem('a/b') ?? '', /一次只建一層/)
  assert.match(newDirNameProblem('  ') ?? '', /請輸入/)
})

test('合法的名字：中文、空白、點開頭、減號開頭、頭尾空白會被 trim', () => {
  for (const ok of ['proj', '新 專案', '.hidden', '-dash', 'a..b', '  padded  ', 'x'.repeat(255)]) {
    assert.equal(newDirNameProblem(ok), null, JSON.stringify(ok))
  }
})
