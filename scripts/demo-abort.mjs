// 強制中止：回合卡住時，輸入框那條鎖上多一顆紅框的「強制中止」，按下去一定解鎖
// （`POST /bots/:id/abort`；`interrupt` 送不出 esc 時會 502 並讓回合繼續卡著）。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-abort.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9352
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-abort', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2400)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name, h = 900) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1280, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const waitFor = async (sel, tries = 60) => { for (let i = 0; i < tries; i++) { if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return true; await sleep(250) } return false }
const lock = () => ev(`(()=>{const l=document.querySelector('.composer-lock');return JSON.stringify({shown:Boolean(l),text:l?.querySelector('span')?.textContent??null,btns:[...(l?.querySelectorAll('button')??[])].map(b=>b.textContent.trim())})})()`)

console.log('== 啟動 bot、送一則訊息讓回合進行中 ==')
await ev("document.querySelector('.bot-row')?.click()")
await sleep(600)
await ev(`[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()`)
await sleep(2500)
console.log(' ', await ev(`(()=>{const t=document.querySelector('.composer textarea');if(!t||t.disabled)return 'composer disabled';const s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(t,'請慢慢想一個很長的答案');t.dispatchEvent(new Event('input',{bubbles:true}));t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'sent'})()`))
// mock 的回覆很快，鎖只會在回合進行中的那一瞬間出現——別等太久。
for (let i = 0; i < 40; i++) { if (await ev(`Boolean(document.querySelector('.composer-lock'))`)) break; await sleep(80) }
console.log('  鎖住時:', await lock())
await shot('358-abort-button')

console.log('== 按下強制中止 ==')
console.log(' ', await ev(`(()=>{const b=[...document.querySelectorAll('.composer-lock button')].find(x=>x.textContent.trim().startsWith('強制中止'));if(!b)return 'MISSING 強制中止';b.click();return 'clicked'})()`))
await sleep(1500)
console.log('  之後:', await lock())
console.log('  notice:', await ev(`[...document.querySelectorAll('.notice')].map(n=>n.textContent.trim().slice(0,60)).join(' | ')`))
console.log('  系統訊息:', await ev(`[...document.querySelectorAll('.msg')].map(m=>m.textContent.trim()).filter(t=>t.includes('強制中止')).slice(-1)[0] ?? '(none)'`))
console.log('  輸入框可用:', await ev(`!document.querySelector('.composer textarea')?.disabled`))
await shot('359-abort-done')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
