import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:5181/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9339','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-plus','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9339/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
console.log('projects:', await ev(`[...document.querySelectorAll('.project-label')].map(e=>e.textContent).join(', ')`))
console.log(await ev(`(()=>{const h=document.querySelectorAll('.project-head')[1]||document.querySelectorAll('.project-head')[0];const b=h.querySelector('.icon-btn.add');if(!b)return 'no + btn';b.click();return 'clicked + on '+h.querySelector('.project-label').textContent})()`)); await sleep(400)
console.log('inline form present:', await ev(`!!document.querySelector('.inline-form form')`), '| preselected:', await ev(`(()=>{const s=document.querySelector('.inline-form select');return s?s.options[s.selectedIndex].textContent:'none'})()`))
await shot('70-plus-bot-inline-form')
await ev(`(()=>{const f=document.querySelector('.inline-form');const set=(el,v)=>{Object.getOwnPropertyDescriptor(el.__proto__,'value').set.call(el,v);el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}))};set(f.querySelector('input[type=text]'),'quick-bot');return 'typed'})()`)
await sleep(200); console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.inline-form button')].find(b=>b.textContent.trim()==='新增');if(!b||b.disabled)return 'submit disabled';b.click();return 'submitted'})()`)); await sleep(800)
console.log('bots now:', await ev(`[...document.querySelectorAll('.bot-name')].map(e=>e.textContent).join(', ')`), '| form closed:', await ev(`!document.querySelector('.inline-form')`))
await shot('71-plus-bot-added')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
