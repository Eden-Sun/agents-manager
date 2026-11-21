
const TAIL_LINES = 16

function normLine(line: string): string {
  const spaced = Array.from(line)
    .map((c) => (/\s/.test(c) || '│┌┐└┘─├┤┬┴┼╭╮╯╰▎▔'.includes(c) ? ' ' : c.toLowerCase()))
    .join('')
  return spaced.split(/\s+/).filter(Boolean).join(' ').replace(/^[❯›»>*●•⏺⎿✻\-—\s]+/, '').trim()
}

export function isDangerousRmScreen(text: string | null | undefined): boolean {
  if (!text) return false
  const lines = text.split('\n').filter((l) => l.trim() !== '')
  const tail = lines.slice(Math.max(0, lines.length - TAIL_LINES)).map(normLine)
  const question = tail.map((l) => l.startsWith('do you want to proceed')).lastIndexOf(true)
  if (question < 0) return false
  const after = tail.slice(question + 1)
  if (!after.some((l) => l.startsWith('1. yes')) || !after.some((l) => l.startsWith('2. no'))) return false
  return tail.slice(0, question).some((l) => l.startsWith('dangerous rm operation'))
}
