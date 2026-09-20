// 主力那列拖曳的截圖（#344）：mock 模式的 vite（VITE_MOCK=1 npx vite --port 5199）＋headless Chrome，用真的滑鼠事件拖。
// `OUT=docs/screenshots/pinned-drag node scripts/pinned-drag-shots.mjs`。暫存 profile 用完就刪。
import { spawn } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const OUT = process.env.OUT ?? 'docs/screenshots/pinned-drag'
const BASE = process.env.BASE ?? 'http://127.0.0.1:5199/'
mkdirSync(OUT, { recursive: true })
const PORT = 9391
const profile = mkdtempSync(join(tmpdir(), 'am-pin-'))
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', `--user-data-dir=${profile}`, '--window-size=1280,800', BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
try {
  for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
  await new Promise((r) => (ws.onopen = r))
  await send('Runtime.enable'); await send('Page.enable')
  const ev = async (expr) => (await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })).result?.value
  const shot = async (name, clip) => { const { data } = await send('Page.captureScreenshot', { format: 'png', clip }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
  await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 800, deviceScaleFactor: 2, mobile: false })
  await send('Page.navigate', { url: BASE }); await sleep(3000)
  const chips = () => ev(`JSON.stringify([...document.querySelectorAll('.unread-chip.pinned[data-bot-id]')].map(c=>{const r=c.getBoundingClientRect();return {n:c.textContent.trim(),x:r.left,y:r.top,w:r.width,h:r.height}}))`).then(JSON.parse)
  const before = await chips()
  console.log('before', before.map((c) => c.n).join(' | '))
  const bar = await ev(`(()=>{const r=document.querySelector('.unread-bar').getBoundingClientRect();return JSON.stringify({x:r.left,y:r.top-6,width:r.width,height:r.height+12,scale:1})})()`).then(JSON.parse)
  await shot('1-before', bar)
  const first = before[0], last = before[before.length - 1]
  const mouse = (type, x, y, extra = {}) => send('Input.dispatchMouseEvent', { type, x, y, button: 'left', buttons: type === 'mouseReleased' ? 0 : 1, clickCount: 1, ...extra })
  await mouse('mousePressed', first.x + first.w / 2, first.y + first.h / 2)
  const tx = last.x + last.w / 2 - 4, ty = last.y + last.h / 2
  for (let i = 1; i <= 12; i++) { await mouse('mouseMoved', first.x + first.w / 2 + ((tx - first.x - first.w / 2) * i) / 12, ty); await sleep(16) }
  await sleep(250)
  await shot('2-dragging-lifted-with-gap', bar)
  await mouse('mouseReleased', tx, ty)
  await sleep(70)
  await shot('3-release-sliding', bar)
  await sleep(500)
  await shot('4-after', bar)
  console.log('after', (await chips()).map((c) => c.n).join(' | '))
  // 減少動態：不位移、不縮放，落點只有主色線。
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] })
  const now = await chips(); const f2 = now[0], l2 = now[now.length - 1]
  await mouse('mousePressed', f2.x + f2.w / 2, f2.y + f2.h / 2)
  for (let i = 1; i <= 12; i++) { await mouse('mouseMoved', f2.x + f2.w / 2 + ((l2.x + l2.w / 2 - f2.x - f2.w / 2) * i) / 12, l2.y + l2.h / 2); await sleep(16) }
  await sleep(250)
  await shot('5-reduced-motion-dragging', bar)
  await mouse('mouseReleased', l2.x + l2.w / 2, l2.y + l2.h / 2)
} finally {
  chrome.kill()
  await sleep(300)
  rmSync(profile, { recursive: true, force: true })
}
process.exit(0)
