import ts from 'typescript'

export interface KeyUse {
  file: string
  line: number
  key: string
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
export interface IgnoreEntry {
  file: string
  text: string
}

const CJK = /[㐀-鿿＀-￯　-〿]/
const I18N_IMPORT = /from\s+['"][^'"]*(?:\/i18n(?:\/index)?(?:\.ts)?|\/shareI18n(?:\.ts)?)['"]/
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

export function findHardcodedCjk(file: string, text: string, ignore: IgnoreEntry[] = []): CjkHit[] {
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
  const excluded = new Set(ignore.filter((e) => e.file === file).map((e) => e.text))
  const add = (n: ts.Node, s: string) => {
    const shown = s.trim().slice(0, 60)
    if (CJK.test(s) && !inI18nCall(n) && !excluded.has(shown)) hits.push({ file, line: lineOf(sf, n), text: shown })
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

export function isUiSource(path: string): boolean {
  if (!/\.(ts|tsx)$/.test(path)) return false
  if (/\.(test|harness)\.(ts|tsx)$/.test(path)) return false
  if (/(^|\/)api\/mock[A-Za-z]*\.ts$|(^|\/)share\/shareMock\.ts$|(^|\/)share\/shareI18n\.ts$/.test(path)) return false
  return !/(^|\/)(testing|i18n|__fixtures__)\//.test(path)
}

export function staleIgnores(ignore: IgnoreEntry[], source: (file: string) => string | undefined): IgnoreEntry[] {
  return ignore.filter((e) => {
    const text = source(e.file)
    if (text === undefined) return true
    return !findHardcodedCjk(e.file, text).some((h) => h.text === e.text)
  })
}
