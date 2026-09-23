// 刪 AGM 的 bot 的第二次確認（issue #406）截圖：mock 模式的 vite（`cd web && VITE_MOCK=1 npx vite --port 5199`）＋headless Chrome。
// `OUT=docs/screenshots/agm-delete-confirm node scripts/agm-delete-shots.mjs`（手機：`MOBILE=1`）。
// daemon 的 409 `supervisor_owned` 用 `__amMock.failNext` 模擬；暫存 profile 用完就刪。
import { spawn } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const OUT = process.env.OUT ?? 'docs/screenshots/agm-delete-confirm'
const BASE = process.env.BASE ?? 'http://127.0.0.1:5199/'
const BOT = process.env.BOT ?? 'am-claude'
mkdirSync(OUT, { recursive: true })
const PORT = 9393
const profile = mkdtempSync(join(tmpdir(), 'am-agm-delete-'))
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', `--user-data-dir=${profile}`, '--window-size=1280,860', BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
try {
  for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
  await new Promise((r) => (ws.onopen = r))
  await send('Runtime.enable'); await send('Page.enable')
  const ev = async (expr) => (await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })).result?.value
  const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
  const mobile = process.env.MOBILE === '1'
  await send('Emulation.setDeviceMetricsOverride', mobile ? { width: 390, height: 844, deviceScaleFactor: 2, mobile: true } : { width: 1280, height: 860, deviceScaleFactor: 2, mobile: false })
  await send('Page.navigate', { url: BASE }); await sleep(3000)
  if (mobile) { await ev(`document.querySelector('button[aria-label*="側邊欄"],button[aria-label*="選單"]')?.click()`); await sleep(600) }
  // 那一列的 ⋯ → 刪除… → 第一個確認框的「刪除」。daemon 對 AGM 的 bot 回 409 supervisor_owned（第一次、不帶 confirm 的那一發）。
  const opened = await ev(`(()=>{const b=document.querySelector('button[aria-label=${JSON.stringify(BOT + ' 的操作')}]');if(!b)return false;b.click();return true})()`)
  console.log('menu', opened); await sleep(300)
  await ev(`[...document.querySelectorAll('.head-menu-item')].find(b=>b.textContent.trim()==='刪除…')?.click()`); await sleep(400)
  await ev(`__amMock.failNext('DELETE','^/bots/[^/?]+$',409,{error:'conflict',reason:'supervisor_owned',name:${JSON.stringify(BOT)},role:'AGM 專案裡的常駐工人',message:'屬於 AGM；確定要刪請帶 ?confirm=supervisor'})`)
  await ev(`[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='刪除')?.click()`); await sleep(900)
  console.log('dialog', await ev(`document.body.textContent.includes('這是 AGM 的 Bot')`))
  await shot(mobile ? 'm1-agm-confirm-390' : '1-agm-confirm-1280')
  await ev(`[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='仍要刪除')?.click()`); await sleep(1200)
  console.log('deleted notice', await ev(`document.querySelector('.notices')?.textContent ?? ''`))
  await shot(mobile ? 'm2-after-confirm-390' : '2-after-confirm-1280')
} finally {
  chrome.kill()
  await sleep(300)
  rmSync(profile, { recursive: true, force: true })
}
