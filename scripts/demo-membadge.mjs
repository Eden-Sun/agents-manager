// 左上角的 herdr RAM 總量（SPEC §15）。Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9387
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-mem','--window-size=1600,900', URL_BASE], { stdio: 'ignore' })
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
const shot = async (name, w = 470, h = 90) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:w,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }

const badge = () => ev(`(() => {
  const b = document.querySelector('.sidebar-head .mem-badge')
  return JSON.stringify({
    shown: Boolean(b),
    text: b?.textContent?.trim() ?? null,
    tip: b?.getAttribute('title')?.split('\\n').filter(Boolean) ?? null,
    partial: Boolean(document.querySelector('.mem-badge.partial')),
  })
})()`)

console.log('idle           ', await badge())
await shot('400-membadge-idle')

await ev("document.querySelector('.bot-row')?.click()"); await sleep(600)
await ev("[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()")
for (let i = 0; i < 40; i += 1) { if (/G|M/.test(String(await ev("document.querySelector('.mem-badge .mem-v')?.textContent ?? ''"))) ) break; await sleep(250) }
await sleep(1600)
console.log('one bot up     ', await badge())
await shot('401-membadge-running')

// a second bot must move the number
await ev("[...document.querySelectorAll('.bot-row')][1]?.click()"); await sleep(600)
await ev("[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()"); await sleep(2200)
console.log('two bots up    ', await badge())

// A host we cannot sample must be flagged, not silently dropped: a total that is quietly
// too low is worse than no total. Add a remote host, then knock it offline.
await ev("document.querySelector('.sidebar-foot .disclosure')?.click()"); await sleep(600)
console.log('fill:', await ev(`(() => {
  const set = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set
  const fields = [...document.querySelectorAll('.modal-body label, .modal-body .field')]
  const put = (label, value) => {
    const f = fields.find((x) => x.textContent.includes(label))
    const i = f?.querySelector('input')
    if (!i) return 'missing ' + label
    set.call(i, value); i.dispatchEvent(new Event('input', { bubbles: true }))
    return 'ok'
  }
  return [put('名稱', 'm4p'), put('ssh 目標', 'm4p@100.112.229.82')].join(',') + ' | fields=' + fields.length
})()`))
await sleep(200)
console.log('submit:', await ev("(() => { const b = [...document.querySelectorAll('.modal-body button')].find((x) => x.textContent.includes('新增並連線')); if (!b) return 'no button'; if (b.disabled) return 'disabled'; b.click(); return 'clicked' })()"))
await sleep(1800)
await ev("document.querySelector('.modal-head .icon-btn')?.click()"); await sleep(400)
console.log('host added     ', await badge())
await ev("window.__amMock.hostDown('m4p')"); await sleep(1200)
console.log('that host down ', await badge())
await shot('402-membadge-partial')

console.log('--- errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
