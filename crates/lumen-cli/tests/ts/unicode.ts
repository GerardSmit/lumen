#!/usr/bin/env node
// Non-ASCII inside erased type text: the blanks keep UTF-8 and UTF-16 lengths, so function
// source text (Function.prototype.toString) and everything after it stays where it was.
type Café = { naïve: "日本語"; emoji: "😀" };
function greet(name: string /* ñ */, extra?: Café["naïve"]): `héllo ${string}` {
  return `héllo ${name}`;
}
const arrow = (x: { ü: number } | "✓"): "→" => "→";
class Größe<Ω extends string = "Ω"> {
  wert: Ω | undefined = undefined as Ω | undefined;
  größe(): number { return 1; }
}
console.log(greet("wörld"), arrow("✓"), new Größe().größe());
console.log(JSON.stringify(greet.toString()));
console.log(JSON.stringify(arrow.toString()));
console.log(JSON.stringify(Größe.toString()));
