import { defaultUrlTransform, type UrlTransform } from 'react-markdown'

export const markdownUrlTransform: UrlTransform = (url, key, node) =>
  key === 'src' && node.tagName === 'img' && /^file:/i.test(url) ? url : defaultUrlTransform(url)

export function readableImagePath(src: string): string {
  try {
    return decodeURI(src)
  } catch {
    return src
  }
}
