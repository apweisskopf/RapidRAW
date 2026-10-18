export const normalizeTag = (tag: string): string =>
  tag
    .split('|')
    .map((part) => part.trim())
    .filter((part) => part.length > 0)
    .join('|');
