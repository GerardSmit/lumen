// Erased statements between lines that would otherwise join into one expression.
const log: string[] = []
let x = 1
type T = number
[1, 2].forEach((n) => log.push("a" + n))
let y = x
interface I { a: number }
(function () { log.push("b") })()
const s = "s"
declare let z: number
`ts`.split("").forEach((c) => log.push(c))
let k = y as number
;(log as string[]).push("c")
let v = x
type A = 1;
type B = 2;
(log).push("d")
if (x) type Local = 1
else log.push("never")
let w = -x
export type { T }
+w
log.push(String(w))
class C {
  a = 1
  private ["c"] = 2
  declare b: number
  static d = 3
  e = 4
  protected *gen() { yield 1 }
}
log.push(String(Object.keys(new C()).length))
console.log(log.join(","))
export {}
