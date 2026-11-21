const FRAME_LINE = /^\s*[╭╮╰╯┌┐└┘├┤┬┴┼╔╗╚╝╠╣─━═╌╍┄┅┈┉\s]*[╭╮╰╯┌┐└┘├┤╔╗╚╝╠╣][╭╮╰╯┌┐└┘├┤┬┴┼╔╗╚╝╠╣─━═╌╍┄┅┈┉\s]*$/
const EDGE = '[│┃║]'
const LEFT_EDGE = new RegExp(`^(\\s*)${EDGE} ?`)
const RIGHT_EDGE = new RegExp(`\\s*${EDGE}$`)

export function reflowForPhone(text: string, maxIndent = 4): string {
  const out: string[] = []
  for (const raw of text.split('\n')) {
    let line = raw.replace(/\s+$/, '')
    if (FRAME_LINE.test(line)) continue
    if (LEFT_EDGE.test(line) && RIGHT_EDGE.test(line) && line.trim().length > 1) {
      line = line.replace(RIGHT_EDGE, '').replace(LEFT_EDGE, '$1')
    } else if (/^\s*[│┃║]$/.test(line)) {
      line = ''
    }
    const indent = /^ */.exec(line)![0].length
    line = ' '.repeat(Math.min(indent, maxIndent)) + line.slice(indent).replace(/ {3,}/g, '  ')
    if (line === '' && (out.length === 0 || out[out.length - 1] === '')) continue
    out.push(line)
  }
  while (out.length && out[out.length - 1] === '') out.pop()
  return out.join('\n')
}
