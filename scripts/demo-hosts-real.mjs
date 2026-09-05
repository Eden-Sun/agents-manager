// SPEC §11.6 against the LIVE daemon: add host m4p from the UI, then browse remote dirs
// under /Users/m4p with the picker. Deliberately does NOT create a project or start a bot.
// Usage: node scripts/demo-hosts-real.mjs [http://127.0.0.1:5198/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5198/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9342
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-hosts-real', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'light' }] })
await send('Page.navigate', { url: URL_BASE })
await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const clickText = (t, sel = 'button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'MISSING '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const type = (sel, value, nth = 0) => ev(`(()=>{const el=document.querySelectorAll(${JSON.stringify(sel)})[${nth}];if(!el)return 'MISSING ${sel}';const d=Object.getOwnPropertyDescriptor(el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype,'value');d.set.call(el,${JSON.stringify(value)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return el.value})()`)

console.log('== 真後端狀態 ==')
console.log('  projects:', await ev(`JSON.stringify([...document.querySelectorAll('.project-label')].map(x=>x.textContent))`))
console.log('  lamps:', await ev(`JSON.stringify([...document.querySelectorAll('.bot-row')].map(r=>[r.querySelector('.bot-name')?.textContent,[...r.querySelector('.lamp').classList].find(c=>c.startsWith('lamp-'))]))`))
console.log('  conn:', await ev(`document.querySelector('.conn-label')?.textContent`))

console.log('== 新增主機 m4p ==')
console.log(' ', await clickText('主機', '.sidebar-foot .disclosure'))
await sleep(400)
console.log('  existing hosts:', await ev(`document.querySelectorAll('.host-row').length`))
console.log('  name:', await type('.hosts-panel input[type=text]', 'm4p', 0))
console.log('  ssh :', await type('.hosts-panel input[type=text]', 'm4p@100.112.229.82', 1))
console.log(' ', await clickText('新增並連線', '.hosts-panel button'))
for (let i = 0; i < 40; i++) { // POST /api/hosts can take ~20s
  await sleep(1000)
  if (await ev(`document.querySelector('.host-result') ? 1 : 0`)) break
}
console.log('  result:', await ev(`document.querySelector('.host-result')?.textContent`))
console.log('  host row:', await ev(`document.querySelector('.host-row')?.textContent`))
await shot('6c-real-host-connected')

console.log('== 遠端目錄選擇器 ==')
console.log(' ', await clickText('新增 Project', '.sidebar-foot .disclosure'))
await sleep(400)
console.log('  host select:', await type('.form select', 'm4p'))
await sleep(300)
console.log(' ', await clickText('瀏覽', '.form button'))
await sleep(3000)
console.log('  picker host:', await ev(`document.querySelector('.dirpicker-host')?.textContent`), '| path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`))
console.log('  entries:', await ev(`JSON.stringify([...document.querySelectorAll('.dirpicker-row .name')].map(x=>x.textContent))`))
await shot('6d-real-remote-dirpicker')
const first = await ev(`document.querySelector('.dirpicker-row .name')?.textContent`)
if (first) {
  console.log(' ', await clickText(first, '.dirpicker-row'))
  await sleep(3000)
  console.log('  picker path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`))
  console.log('  entries:', await ev(`JSON.stringify([...document.querySelectorAll('.dirpicker-row .name')].map(x=>x.textContent))`))
  await shot('6e-real-remote-dirpicker-sub')
}
console.log(' ', await clickText('取消'))  // 不建立 Project：真後端上留給後端 agent 驗收

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)
