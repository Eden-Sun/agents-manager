// ↑/↓ in the sidebar listbox and ⌥↑/⌥↓ from anywhere both switch bot.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9373
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-botkeys','--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1440,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:460,height:620,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

const VK = { ArrowUp: 38, ArrowDown: 40 }
const key = async (k, { alt = false } = {}) => {
  const base = { key: k, code: k, windowsVirtualKeyCode: VK[k], nativeVirtualKeyCode: VK[k], modifiers: alt ? 1 : 0 }
  await send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...base })
  await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base })
  await sleep(220)
}

const state = () => ev(`(() => ({
  selected: document.querySelector('.bot-row.selected .bot-name')?.textContent?.trim() ?? null,
  focused: document.activeElement?.closest?.('.bot-row')?.querySelector('.bot-name')?.textContent?.trim()
    ?? document.activeElement?.tagName?.toLowerCase() ?? null,
  header: document.querySelector('.main-head .main-title > strong')?.textContent ?? null,
  order: [...document.querySelectorAll('.bot-row .bot-name')].map((e) => e.textContent.trim()).join(','),
}))()`)

console.log('order:', (await state()).order)

// --- 1. plain arrows inside the sidebar listbox -----------------------------
await ev("document.querySelector('.bot-row')?.focus()")
await ev("document.querySelector('.bot-row')?.click()")
await sleep(400)
console.log('start        ', JSON.stringify(await state()))
await key('ArrowDown'); console.log('↓ in sidebar ', JSON.stringify(await state()))
await key('ArrowDown'); console.log('↓ in sidebar ', JSON.stringify(await state()))
await key('ArrowUp');   console.log('↑ in sidebar ', JSON.stringify(await state()))
await shot('340-botkeys-sidebar')

// --- 2. ⌥↑/⌥↓ while typing in the composer ---------------------------------
await ev("document.querySelector('.composer textarea')?.focus()")
await ev(`(() => {
  const t = document.querySelector('.composer textarea')
  if (!t) return 'no composer'
  const set = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set
  set.call(t, '打到一半的字')
  t.dispatchEvent(new Event('input', { bubbles: true }))
  t.setSelectionRange(3, 3)
  return 'typed'
})()`)
console.log('composer     ', JSON.stringify(await state()))
await key('ArrowDown')
console.log('plain ↓      ', JSON.stringify(await state()), '| caret/text kept:', await ev(`(() => {
  const t = document.querySelector('.composer textarea')
  return t ? \`\${JSON.stringify(t.value)}\` : 'gone'
})()`))
await key('ArrowDown', { alt: true }); console.log('⌥↓ composer  ', JSON.stringify(await state()))
await key('ArrowUp', { alt: true });   console.log('⌥↑ composer  ', JSON.stringify(await state()))

// --- 3. wrap-around, and ⌥ inside a row still reorders ----------------------
// Selecting a bot hands focus to the composer, so re-focus the row *after* that settles —
// otherwise the arrow lands in the textarea and this tests nothing.
const focusFirstRow = async () => {
  await ev("document.querySelector('.bot-row')?.click()")
  await sleep(400)
  await ev("document.querySelector('.bot-row')?.focus()")
  await sleep(150)
}
await focusFirstRow()
const before = (await state()).order
await key('ArrowUp')
console.log('↑ from first ', JSON.stringify(await state()), '(wraps to the last)')
await focusFirstRow()
await key('ArrowDown', { alt: true })
const after = (await state()).order
console.log('⌥↓ in a row  reordered:', before !== after, '|', before, '->', after)

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)
