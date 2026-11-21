
export function mainScriptFromHtml(html: string): string | null {
  for (const m of html.matchAll(/<script\b[^>]*>/gi)) {
    const tag = m[0]
    if (!/\btype\s*=\s*["']module["']/i.test(tag)) continue
    const src = /\bsrc\s*=\s*["'](\/assets\/[^"']+\.js)["']/i.exec(tag)
    if (src) return src[1]
  }
  return null
}

export function loadedMainScript(doc: Pick<Document, 'querySelector'>): string | null {
  const el = doc.querySelector('script[type="module"][src*="/assets/"]')
  return el?.getAttribute('src') ?? null
}

export function isStaleBundle(loaded: string | null, served: string | null): boolean {
  return loaded !== null && served !== null && loaded !== served
}

export function shouldCheck(now: number, lastCheckAt: number | null, gapMs = 30_000): boolean {
  return lastCheckAt === null || now - lastCheckAt >= gapMs
}

let lastCheckAt: number | null = null
let dismissedFor: string | null = null

export function resetStaleBundleForTest(): void {
  lastCheckAt = null
  dismissedFor = null
}

export async function checkServedBundle(loaded: string | null): Promise<string | null> {
  if (loaded === null || !shouldCheck(Date.now(), lastCheckAt)) return null
  lastCheckAt = Date.now()
  try {
    const res = await fetch('/index.html', { cache: 'no-store' })
    if (!res.ok) return null
    const served = mainScriptFromHtml(await res.text())
    return isStaleBundle(loaded, served) && served !== dismissedFor ? served : null
  } catch {
    return null
  }
}

export function dismissStaleBundle(version: string): void {
  dismissedFor = version
}
