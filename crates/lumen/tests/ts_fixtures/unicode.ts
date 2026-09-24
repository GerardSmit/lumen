// Non-ASCII text before functions: offsets are UTF-8 bytes. Ünïcödé ✓ 🚀
const greeting: string = "héllo wörld 🌍";

export function shout(s: string): string {
  return s.toUpperCase() + "！";
}

/* 日本語のコメント */
export const len = (s: string): number => s.length;
