// 對話倒回（#405）的截圖：mock 模式的 vite（`cd web && VITE_MOCK=1 npx vite --port 5198`）＋headless Chrome。
// `OUT=docs/screenshots/rewind node scripts/rewind-shots.mjs`。暫存 profile 用完就刪。
import { spawn } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const OUT = process.env.OUT ?? 'docs/screenshots/rewind'
const BASE = process.env.BASE ?? 'http://127.0.0.1:5198/'
const BOT = process.env.BOT ?? 'am-claude'
mkdirSync(OUT, { recursive: true })
const PORT = 9392
const profile = mkdtempSync(join(tmpdir(), 'am-rewind-'))
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
  const shot = async (name) => { const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
  const mobile = process.env.MOBILE === '1'
  await send('Emulation.setDeviceMetricsOverride', mobile ? { width: 390, height: 844, deviceScaleFactor: 2, mobile: true } : { width: 1280, height: 860, deviceScaleFactor: 2, mobile: false })
  if (mobile) await send('Emulation.setEmulatedMedia', { features: [{ name: 'hover', value: 'none' }] })
  await send('Page.navigate', { url: BASE }); await sleep(3000)
  // 選 bot：點側欄（或手機的切換）裡名字剛好是 BOT 的那一列。
  const picked = await ev(`(()=>{const el=[...document.querySelectorAll('button,[role=button],a,li,div')].find(e=>e.children.length<4&&e.textContent.trim()===${JSON.stringify(BOT)});if(!el)return false;el.click();return true})()`)
  console.log('picked', picked)
  await sleep(1200)
  // 沒在跑就先啟動（手機寬度下 mock 的初始狀態不一定是跑著的）。
  if (await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='啟動');if(b){b.click();return true}return false})()`)) await sleep(2500)
  // 送兩則，讓畫面上有使用者訊息可以倒。
  for (const text of ['幫我列出 web/src 底下的元件', '改成只列 .tsx 的（問錯了，其實想問 hooks）']) {
    await ev(`(()=>{const t=document.querySelector('textarea');const set=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;set.call(t,${JSON.stringify(text)});t.dispatchEvent(new Event('input',{bubbles:true}));return true})()`)
    await sleep(200)
    await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^送出|傳送|Send/.test(b.textContent.trim())||b.getAttribute('aria-label')==='送出');if(b){b.click();return true}const t=document.querySelector('textarea');t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return false})()`)
    await sleep(5000)
  }
  const users = await ev(`document.querySelectorAll('.msg.user').length`)
  console.log('user messages', users)
  // 滑過最後一則使用者訊息：按鈕出現。
  const rect = await ev(`(()=>{const ms=[...document.querySelectorAll('.msg.user')];const m=ms[ms.length-1];m.scrollIntoView({block:'center'});const r=m.getBoundingClientRect();return JSON.stringify({x:r.left+r.width/2,y:r.top+10})})()`).then(JSON.parse)
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: rect.x, y: rect.y })
  await sleep(300)
  await shot(mobile ? 'm1-button' : '1-hover-shows-button')
  await ev(`(()=>{const ms=[...document.querySelectorAll('.msg.user')];ms[ms.length-1].querySelector('.msg-rewind').click();return true})()`)
  await sleep(400)
  await shot(mobile ? 'm2-confirm' : '2-confirm')
  await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='倒回');b.click();return true})()`)
  await sleep(1200)
  await shot(mobile ? 'm3-after' : '3-after-rewound-and-refilled')
  console.log('composer', await ev(`document.querySelector('textarea').value`))
  console.log('rewound', await ev(`document.querySelectorAll('.msg.rewound').length`))
} finally {
  chrome.kill()
  await sleep(300)
  rmSync(profile, { recursive: true, force: true })
}
process.exit(0)
