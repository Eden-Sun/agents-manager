export const AUTO_TRANSLATE_RECENT = 10

export function usd(n: number): string {
  return n >= 0.01 ? `$${n.toFixed(3)}` : `$${n.toFixed(5)}`
}

export function translationModelName(model: string): string {
  if (isLocalTranslation(model)) return 'OpenCC'
  const openai = /^gpt-\d+(?:\.\d+)*-([^-]+)/.exec(model)
  const first = openai ? openai[1] : (model.replace(/^claude-/, '').split('-')[0] ?? '')
  return first.charAt(0).toUpperCase() + first.slice(1)
}

export function isLocalTranslation(model: string): boolean {
  return model.startsWith('opencc')
}
