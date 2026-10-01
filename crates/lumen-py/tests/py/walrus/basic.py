if (n := len("hello")) > 3:
    print("long", n)

data = [1, 2, 3, 4]
while (item := data.pop()) > 2:
    print("popped", item)
print(item, data)

print((a := 5) + a)
print((b := [1, 2, 3]), b)
x = (y := 10) * 2
print(x, y)

def f(v):
    if (r := v * 2) > 10:
        return r
    return -r
print(f(3), f(8))

buf = iter(["a", "b", "", "c"])
while (chunk := next(buf)):
    print(chunk)

print([p for p in range(10) if (sq := p * p) > 20][:2], sq)
(c := 3)
print(c)
t = (u := 1, 2)
print(t, u)
lst = [(k := 1), k + 1, k + 2]
print(lst)
print(f"{(z := 7)} {z}")
d = {"key": (e := "val")}
print(d, e)
if (m := {"a": 1}.get("a")) is not None:
    print("found", m)
if (m := {"a": 1}.get("b")) is None:
    print("missing", m)
def g(a=(w := 4)):
    return a
print(g(), w)
print(any((hit := i) % 7 == 0 for i in range(1, 20)), hit)
s = lambda: (q := 1)
print(s())
print((lambda: (inner := 5) + inner)())
