// Click the bot name in the header to rename it in place (no trip to the settings panel).
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9385
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-rename','--window-size=1600,900', URL_BASE], { stdio: 'ignore' })
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
const shot = async (name, h = 130) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:1600,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

/** React listens at the root, so a bubbling synthetic event is enough for both. */
const setValue = (text) => ev(`(() => {
  const i = document.querySelector('.bot-name-input')
  if (!i) return 'not editing'
  const set = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  set.call(i, ${JSON.stringify(text)})
  i.dispatchEvent(new Event('input', { bubbles: true }))
  return 'typed'
})()`)
const key = (k) => ev(`(() => {
  const i = document.querySelector('.bot-name-input')
  if (!i) return 'not editing'
  i.dispatchEvent(new KeyboardEvent('keydown', { key: ${JSON.stringify(k)}, bubbles: true, cancelable: true }))
  return 'key ' + ${JSON.stringify(k)}
})()`)

const state = () => ev(`(() => JSON.stringify({
  headerName: document.querySelector('.bot-name-btn strong')?.textContent ?? null,
  editing: Boolean(document.querySelector('.bot-name-input')),
  value: document.querySelector('.bot-name-input')?.value ?? null,
  invalid: Boolean(document.querySelector('.bot-name-input.bad')),
  sidebar: [...document.querySelectorAll('.bot-row .bot-name')].map((e) => e.textContent.trim().split('\\n')[0]).join(','),
}))()`)

await ev("document.querySelector('.bot-row')?.click()"); await sleep(900)
console.log('start          ', await state())

// --- click the name → it becomes an input --------------------------------
await ev("document.querySelector('.bot-name-btn')?.click()"); await sleep(300)
console.log('click name     ', await state())
await shot('390-rename-editing')

// --- an invalid name is flagged and never sent ---------------------------
await setValue('bad name'); await sleep(200)
console.log('space in name  ', await state())
await key('Enter'); await sleep(500)
console.log('Enter (invalid)', await state(), '← reverted, not saved')

// --- rename for real -----------------------------------------------------
await ev("document.querySelector('.bot-name-btn')?.click()"); await sleep(250)
await setValue('前端-1'); await sleep(200)
await key('Enter'); await sleep(700)
console.log('Enter (valid)  ', await state(), '← header and sidebar both updated')
await shot('391-rename-done')

// --- Esc discards --------------------------------------------------------
await ev("document.querySelector('.bot-name-btn')?.click()"); await sleep(250)
await setValue('丟掉的名字'); await sleep(200)
await key('Escape'); await sleep(400)
console.log('Escape         ', await state(), '← draft discarded')

// --- sidebar: click the name of the *selected* row ------------------------
console.log('\n--- sidebar ---')
const row = () => ev(`(() => {
  const r = document.querySelector('.bot-row.selected')
  return JSON.stringify({
    selected: r?.querySelector('.bot-name, .bot-name-input')?.textContent?.trim().split('\\n')[0] ?? null,
    renamable: Boolean(document.querySelector('.bot-row.selected .bot-name.renamable')),
    editingInRow: Boolean(document.querySelector('.bot-row .bot-name-input')),
    names: [...document.querySelectorAll('.bot-row .bot-name, .bot-row .bot-name-input')].map((e) => (e.value ?? e.textContent).trim().split('\\n')[0]).join(','),
  })
})()`)
console.log('selected row   ', await row())

// an *unselected* row: clicking its name must select it, not open an editor
await ev("[...document.querySelectorAll('.bot-row')].find((r) => !r.classList.contains('selected'))?.querySelector('.bot-name')?.click()")
await sleep(700)
console.log('click other row', await row(), '← selection moved, no editor')

// the selected row: now the name is the rename target
await ev("document.querySelector('.bot-row.selected .bot-name')?.click()"); await sleep(350)
console.log('click its name ', await row())
await shot('392-rename-sidebar', 400)
await setValue('後端-2'); await sleep(200)
await key('Enter'); await sleep(700)
console.log('Enter          ', await row(), '←', await state())

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
