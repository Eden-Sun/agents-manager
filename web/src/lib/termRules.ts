const RULE_LINE = /^(\s*)([─━╌╍┄┅┈┉═]{40,})\s*$/

export function fitRules(text: string, keep = 32): string {
  if (!/[─━╌╍┄┅┈┉═]{40}/.test(text)) return text
  return text
    .split('\n')
    .map((line) => {
      const m = RULE_LINE.exec(line)
      return m ? m[1] + m[2].slice(0, keep) : line
    })
    .join('\n')
}
