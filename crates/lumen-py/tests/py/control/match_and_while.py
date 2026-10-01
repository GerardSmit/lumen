def kind(v):
    match v:
        case 0:
            return "zero"
        case 1 | 2:
            return "one-two"
        case int() if v < 0:
            return "negint"
        case int():
            return "int"
        case str() as s:
            return "str:" + s
        case [x, y]:
            return "pair %s %s" % (x, y)
        case [x, *rest]:
            return "list head %s rest %s" % (x, rest)
        case {"k": val}:
            return "dict k=%s" % val
        case None:
            return "none"
        case _:
            return "other"
for v in (0, 2, -4, 9, "hi", [1, 2], [1, 2, 3], {"k": 7}, None, 2.5, []):
    print(repr(v), kind(v))

i = 0
while i < 10:
    i += 1
    if i % 2 == 0:
        continue
    if i > 7:
        break
    print("odd", i)
print(i)
x = 10
while x:
    x //= 3
    print(x)
print(sum(range(101)))
for _ in range(3):
    pass
print(_)
