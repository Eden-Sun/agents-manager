import test from 'node:test'
import assert from 'node:assert/strict'
import { formatSize, sizeLabel } from './attachmentsHelpers'

const MB = 1024 * 1024

test('KB 進位到 1024 時要寫成 MB，不寫 1024 KB（檔案大小一行）', () => {
  // 1023.5 KB 以上四捨五入會成 1024：卡片上不能出現「1024 KB」。
  assert.equal(formatSize(MB - 1), '1.0 MB')
  assert.equal(sizeLabel(MB - 1, 3 * MB), '1.0 MB（原 3.0 MB）')
})
