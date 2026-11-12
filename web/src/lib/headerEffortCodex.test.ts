/**
 * 標題列的 codex 強度不能顯示成「—」：遠端 run（沒有終端 footer）只剩 daemon 寫進 status_line 的那一行，
 * 也要讀得出 low（pt-hub／mac_builder SOL 實例：DB 有 runtime_effort=low，標題列卻沒有）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { codexHeaderEffort } from './headerEffort.ts'
import { parseCodexModelLine, splitCodexTerminalFooter } from './codexTerminalFooter.ts'

const STATUS_LINE = 'GPT-6.1-Sol low · ~/project/pt-hub · Context 42% used · weekly 89% left · ← for agents'
const base = { terminalModelLine: null, statusEffort: null, statusLineModelLine: null, runEffort: null, slugEffort: null, parse: parseCodexModelLine }

test('遠端 SOL：沒有終端 footer、status_json 沒有強度，status_line 有 low → 顯示 low', () => {
  const modelLine = splitCodexTerminalFooter(STATUS_LINE).footer?.modelLine ?? null
  assert.ok(modelLine, 'status_line 要拆得出 modelLine')
  assert.equal(codexHeaderEffort({ ...base, statusLineModelLine: modelLine }), 'low')
})

test('沒有任何來源但有 run 實際啟動值 low（DB runtime_effort）→ low，不是 null', () => {
  assert.equal(codexHeaderEffort({ ...base, runEffort: 'low' }), 'low')
})

test('優先序：終端 footer > status_json > status_line > run 啟動值 > slug', () => {
  const sl = { ...base, statusLineModelLine: 'GPT-6.1-Sol high', runEffort: 'low', slugEffort: 'medium' }
  assert.equal(codexHeaderEffort({ ...sl, terminalModelLine: 'GPT-6.1-Sol xhigh' }), 'xhigh')
  assert.equal(codexHeaderEffort({ ...sl, statusEffort: 'max' }), 'max')
  assert.equal(codexHeaderEffort(sl), 'high')
  assert.equal(codexHeaderEffort({ ...sl, statusLineModelLine: null }), 'low')
  assert.equal(codexHeaderEffort({ ...sl, statusLineModelLine: null, runEffort: null }), 'medium')
})

test('不是強度詞的東西不當強度（路徑、標題、空字串）', () => {
  assert.equal(codexHeaderEffort({ ...base, statusLineModelLine: 'GPT-6.1-Sol · ~/project/x', statusEffort: 'pro' }), null)
  assert.equal(codexHeaderEffort({ ...base, runEffort: '' }), null)
})
