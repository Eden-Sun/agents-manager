// The header slot that said `執行中 ▾` now carries the pane id and toggles the run detail.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9377
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-panetoggle','--window-size=1600,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1600,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name, h = 170) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:1600,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

const state = () => ev(`(() => {
  const slot = document.querySelector('.main-head .main-status')
  return JSON.stringify({
    slotText: slot?.textContent?.trim() ?? null,
    slotTitle: slot?.getAttribute('title') ?? null,
    lampTitle: document.querySelector('.main-head .lamp')?.getAttribute('title') ?? null,
    detailOpen: Boolean(document.querySelector('.run-debug')),
    detailChips: [...document.querySelectorAll('.run-debug .copy-chip, .run-debug [class*=chip]')].map((c) => c.textContent.trim()).join(' | ') || null,
  })
})()`)

await ev("document.querySelector('.bot-row')?.click()"); await sleep(700)
console.log('stopped bot |', await state())

await ev("[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()")
for (let i = 0; i < 60; i += 1) { if (await ev("Boolean(document.querySelector('.main-head .main-status'))")) break; await sleep(250) }
await sleep(400)
console.log('running bot |', await state())
await shot('360-pane-toggle-closed')

await ev("document.querySelector('.main-head .main-status')?.click()"); await sleep(400)
console.log('after click |', await state())
await shot('361-pane-toggle-open')

await ev("document.querySelector('.main-head .main-status')?.click()"); await sleep(400)
console.log('click again |', await state())

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
