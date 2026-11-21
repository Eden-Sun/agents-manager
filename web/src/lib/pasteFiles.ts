export function pastedFiles(dt: { files?: ArrayLike<File> | null; types?: ArrayLike<string> | null; getData(type: string): string } | null | undefined): File[] {
  const files = Array.from(dt?.files ?? [])
  if (files.length === 0) return []
  const types = Array.from(dt?.types ?? [])
  const officeText = types.includes('text/rtf') && dt!.getData('text/plain').trim() !== '' && files.every((file) => file.type.startsWith('image/'))
  return officeText ? [] : files
}
