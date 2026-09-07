import test from 'node:test'
import assert from 'node:assert/strict'
import { typeAlongside } from './alongside.ts'

/** 記下每一通 IO，好斷言「送出去的就是使用者打的那一整段」。 */
function recorder(ok = true) {
  const calls: { botId: string; text: string; enter: boolean }[] = []
  return {
    calls,
    io: {
      sendText: (botId: string, text: string, enter: boolean) => {
        calls.push({ botId, text, enter })
        return Promise.resolve(ok)
      },
    },
  }
}

test('多行文字一次整段送出，換行原樣保留', async () => {
  const body = '第一行\n第二行\n\n第四行 with spaces'
  const r = recorder()
  assert.equal(await typeAlongside(r.io, 'b1', body), true)
  assert.deepEqual(r.calls, [{ botId: 'b1', text: body, enter: true }])
})

test('前後空白修掉，但內容裡的換行不動', async () => {
  const r = recorder()
  await typeAlongside(r.io, 'b1', '  上\n下  ')
  assert.equal(r.calls[0].text, '上\n下')
})

test('空白內容不送任何東西', async () => {
  const r = recorder()
  assert.equal(await typeAlongside(r.io, 'b1', '   \n  '), false)
  assert.equal(r.calls.length, 0)
})

test('送失敗就回 false，輸入框留著', async () => {
  const r = recorder(false)
  assert.equal(await typeAlongside(r.io, 'b1', '一句話'), false)
})
