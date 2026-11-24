import test from 'node:test'
import assert from 'node:assert/strict'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'

const SRC = new URL('.', import.meta.url).pathname

function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name)
    if (statSync(path).isDirectory()) return sourceFiles(path)
    return /\.(ts|tsx)$/.test(name) && !/\.test\.(ts|tsx)$/.test(name) ? [path] : []
  })
}

const FILES = sourceFiles(SRC)
const NATIVE_DIALOG = /(?<![\w$.:'"`])(?:window\.|globalThis\.)?(alert|confirm|prompt)\s*\(/

test('production 程式不呼叫瀏覽器原生 alert／confirm／prompt', () => {
  const hits = FILES.flatMap(path => {
    const lines = readFileSync(path, 'utf8').split('\n')
    return lines.flatMap((line, i) => NATIVE_DIALOG.test(line) && !/\b(private|public|async)\s+(prompt|confirm|alert)\s*\(|\bdef\s|\bfunction\s+(prompt|confirm|alert)\b/.test(line)
      ? [`${path}:${i + 1}: ${line.trim()}`]
      : [])
  })
  assert.deepEqual(hits, [], '請改用站內對話框（ConfirmDialog 等），不要用原生對話框')
})

test('每個 new Terminal() 都帶 linkHandler，且 allowNonHttpProtocols 為 false', () => {
  const offenders: string[] = []
  for (const path of FILES) {
    const src = readFileSync(path, 'utf8')
    if (!/new Terminal\(/.test(src)) continue
    if (!/linkHandler\s*:/.test(src) || !/allowNonHttpProtocols\s*:\s*false/.test(src)) offenders.push(path)
  }
  assert.ok(FILES.some(path => /new Terminal\(/.test(readFileSync(path, 'utf8'))), '找不到任何 new Terminal(，守門規則可能失效')
  assert.deepEqual(offenders, [], 'xterm 預設 linkHandler 會呼叫 confirm()；請設 linkHandler 並 allowNonHttpProtocols: false')
})
