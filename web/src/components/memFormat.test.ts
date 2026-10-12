import test from 'node:test'
import assert from 'node:assert/strict'
import { humanBytes } from './memFormat'

const MiB = 1024 * 1024
const GiB = 1024 * MiB

test('humanBytes 單位進位：四捨五入到 1024M 要改寫成 G，不寫 1024M', () => {
  // 1023.6 MiB 四捨五入是 1024M：RAM 格子上不能出現「1024M」。
  assert.equal(humanBytes(Math.round(1023.6 * MiB)), '1.0G')
})

test('humanBytes 9.96G 進位到 10 時不寫成 10.0G（≥10 的寫法是整數）', () => {
  assert.equal(humanBytes(Math.round(9.96 * GiB)), '10G')
})

test('humanBytes KB→MB 進位：1023.5K 以上四捨五入到 1024K 要改寫成 M，不寫 1024K', () => {
  // 19c6c5f6 只補了 M→G；K→M 的邊界（1048575 位元組 ＝ 1023.999K）還是印「1024K」。
  assert.equal(humanBytes(1048575), '1M')
  assert.equal(humanBytes(1048064), '1M')
  assert.equal(humanBytes(1047551), '1023K', '還沒到 1023.5K 的仍是 K')
})
