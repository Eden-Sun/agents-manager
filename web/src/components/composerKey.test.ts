import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

/**
 * `sending` 是 `Composer` 的本地 state，而 `ChatPanel` 的 botId 是從 store 讀的——換 bot 不會重建元件。
 * 少了這個 key，A 的送出還在飛時切到 B，B 的輸入框、📎 與送出鍵會一起被鎖住（issue #496）。
 * 這條只認得出「key 不見了」，清托盤那一半由 `store/queuedSend.test.ts` 守著。
 */
test('Composer 掛 key={botId}：切 bot 時送出中的狀態跟著重來', () => {
  const src = readFileSync(new URL('./ChatPanel.tsx', import.meta.url), 'utf8')
  assert.match(src, /<Composer\s+key=\{botId\}/)
})
