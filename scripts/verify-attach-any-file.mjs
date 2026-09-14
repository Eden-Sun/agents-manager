// 驗證：暫存區與托盤收任意檔（mock UI, 5174）。OUT=dir node verify.mjs
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
const OUT = process.env.OUT ?? '/tmp/am-attach-any/shots'
mkdirSync(OUT, { recursive: true })
const URL_BASE = 'http://127.0.0.1:5174/'
const PORT = 9391
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-attach-any-profile', '--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable'); await send('DOM.enable')
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description); return r.result?.value }
const shot = async (name) => { await sleep(500); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
const setFiles = async (selector, files) => {
  const { root } = await send('DOM.getDocument')
  const { nodeId } = await send('DOM.querySelector', { nodeId: root.nodeId, selector })
  if (!nodeId) throw new Error('no input for ' + selector)
  await send('DOM.setFileInputFiles', { nodeId, files })
}
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
// 開一個 bot，托盤才有收件對象
await ev(`document.querySelector('.bot-row')?.click()`); await sleep(1200)
// 暫存區展開
await ev(`document.querySelector('.shelf-handle')?.click()`); await sleep(500)
await setFiles('.shelf input[type=file]', ['/tmp/am-attach-any/note.md', '/tmp/am-attach-any/data.csv', '/tmp/am-attach-any/shot.png'])
await sleep(900)
console.log('shelf cards:', await ev(`[...document.querySelectorAll('.shelf-card-name')].map(n=>n.textContent).join(' | ')`))
console.log('file glyphs:', await ev(`document.querySelectorAll('.shelf-card-file').length`))
console.log('thumbnails:', await ev(`document.querySelectorAll('.shelf-card-main img').length`))
await shot('shelf-any-file')
// 點兩張卡放進對話托盤
await ev(`[...document.querySelectorAll('.shelf-card-main')].slice(0,3).forEach(b=>b.click())`)
await sleep(1200)
console.log('tray names:', await ev(`[...document.querySelectorAll('.attach-thumb-name')].map(n=>n.textContent).join(' | ')`))
console.log('tray glyphs:', await ev(`document.querySelectorAll('.attach-thumb-file').length`))
await shot('tray-any-file')
chrome.kill(); process.exit(0)
