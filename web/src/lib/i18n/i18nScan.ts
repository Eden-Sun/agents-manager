/**
 * 多語系的靜態掃描（只給 `i18nDict.test.ts` 與 `scripts/i18n-report.ts` 用，不進 bundle）。
 * 用 TypeScript AST 找 `t('…')`／`msg('…')` 的 key 與插值名稱，以及還沒抽出去的寫死中文。
 */
import ts from 'typescript'

export interface KeyUse {
  file: string
  line: number
  key: string
  /** 第二個參數是物件字面值時的插值名稱；不是字面值（變數、含 spread）為 null＝不檢查。 */
  vars: string[] | null
}
export interface Problem {
  file: string
  line: number
  message: string
}
export interface CjkHit {
  file: string
  line: number
  text: string
}

const CJK = /[㐀-鿿＀-￯　-〿]/
const I18N_IMPORT = /from\s+['"][^'"]*\/i18n(?:\/index)?(?:\.ts)?['"]/
const PLACEHOLDER = /\{([A-Za-z_]\w*)\}/g

export function placeholderNames(s: string): string[] {
  const out: string[] = []
  for (const m of s.matchAll(PLACEHOLDER)) if (!out.includes(m[1])) out.push(m[1])
  return out
}

function parse(file: string, text: string): ts.SourceFile {
  return ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, file.endsWith('x') ? ts.ScriptKind.TSX : ts.ScriptKind.TS)
}
const lineOf = (sf: ts.SourceFile, node: ts.Node) => sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1
const literalText = (n: ts.Node | undefined): string | null =>
  n && (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) ? n.text : null

/** `t(…)`、`msg(…)`、`t.x(…)`（句子裡塞元件）。`tr` 是動態查詢、不檢查。 */
const isKeyCall = (e: ts.Expression): boolean =>
  (ts.isIdentifier(e) && (e.text === 't' || e.text === 'msg')) || (ts.isPropertyAccessExpression(e) && ts.isIdentifier(e.expression) && e.expression.text === 't' && e.name.text === 'x')

function varNames(arg: ts.Expression | undefined): string[] | null {
  if (!arg || !ts.isObjectLiteralExpression(arg)) return arg ? null : []
  const out: string[] = []
  for (const p of arg.properties) {
    if (ts.isSpreadAssignment(p) || !p.name) return null
    if (ts.isIdentifier(p.name) || ts.isStringLiteral(p.name)) out.push(p.name.text)
    else return null
  }
  return out
}

/** 不是來自 lib/i18n 的檔案（沒 import）不掃：只有 import 了 i18n 模組的檔案裡的 `t(…)`／`msg(…)` 才算。 */
export function scanKeys(file: string, text: string): { uses: KeyUse[]; problems: Problem[] } {
  const uses: KeyUse[] = []
  const problems: Problem[] = []
  if (!I18N_IMPORT.test(text)) return { uses, problems }
  const sf = parse(file, text)
  const visit = (n: ts.Node) => {
    if (ts.isCallExpression(n) && isKeyCall(n.expression)) {
      const key = literalText(n.arguments[0])
      if (key === null) {
        problems.push({ file, line: lineOf(sf, n), message: `${n.expression.getText(sf)}() 的 key 必須是字面字串（動態文字用 msg() 標記、tr() 查）` })
      } else {
        const vars = varNames(n.arguments[1])
        uses.push({ file, line: lineOf(sf, n), key, vars })
        if (vars) {
          const want = placeholderNames(key)
          const missing = want.filter((w) => !vars.includes(w))
          const extra = vars.filter((v) => !want.includes(v))
          if (missing.length) problems.push({ file, line: lineOf(sf, n), message: `「${key}」缺插值 ${missing.map((m) => `{${m}}`).join(' ')}` })
          if (extra.length) problems.push({ file, line: lineOf(sf, n), message: `「${key}」多傳了 ${extra.join(', ')}（key 裡沒有對應的 {…}）` })
        }
      }
    }
    ts.forEachChild(n, visit)
  }
  visit(sf)
  return { uses, problems }
}

/**
 * 還沒抽出去的寫死中文：JSX 文字、字串字面值、樣板字串的各段，只要含中日韓字元或全形標點。
 * 排除 `t()`／`msg()`／`tr()` 的引數與 import／export 的路徑。註解不算（AST 本來就看不到）。
 * 刻意不翻的（語言自稱、要跟 daemon 回的文字比對的字串）在該行行尾、或獨立的上一行寫 `// i18n-ignore`（JSX 文字旁放一個含 i18n-ignore 的 JSX 註解）。
 */
export function findHardcodedCjk(file: string, text: string): CjkHit[] {
  if (!CJK.test(text)) return []
  const sf = parse(file, text)
  const hits: CjkHit[] = []
  const inI18nCall = (n: ts.Node): boolean => {
    for (let child = n, p = n.parent; p; child = p, p = p.parent) {
      if (ts.isCallExpression(p) && p.arguments[0] === child && (isKeyCall(p.expression) || (ts.isIdentifier(p.expression) && p.expression.text === 'tr') || (ts.isPropertyAccessExpression(p.expression) && p.expression.name.text === 'tr'))) return true
      if (ts.isImportDeclaration(p) || ts.isExportDeclaration(p)) return true
    }
    return false
  }
  const lines = text.split('\n')
  const ignored = (n: ts.Node) => {
    const line = lineOf(sf, n)
    const prev = (lines[line - 2] ?? '').trim()
    return lines[line - 1].includes('i18n-ignore') || (/^(\/\/|\/\*|\{\/\*)/.test(prev) && prev.includes('i18n-ignore'))
  }
  const add = (n: ts.Node, s: string) => {
    if (CJK.test(s) && !inI18nCall(n) && !ignored(n)) hits.push({ file, line: lineOf(sf, n), text: s.trim().slice(0, 60) })
  }
  const visit = (n: ts.Node) => {
    if (ts.isJsxText(n)) add(n, n.text)
    else if (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) add(n, n.text)
    else if (ts.isTemplateHead(n) || ts.isTemplateMiddle(n) || ts.isTemplateTail(n)) add(n, n.text)
    ts.forEachChild(n, visit)
  }
  visit(sf)
  return hits
}

/** 要翻譯的原始碼：略過測試、字典本身、i18n 工具、mock 假 daemon 的資料與測試輔助。 */
export function isUiSource(path: string): boolean {
  if (!/\.(ts|tsx)$/.test(path)) return false
  if (/\.(test|harness)\.(ts|tsx)$/.test(path)) return false
  if (/(^|\/)api\/mock[A-Za-z]*\.ts$|(^|\/)share\/shareMock\.ts$/.test(path)) return false
  return !/(^|\/)(testing|i18n|__fixtures__)\//.test(path)
}
