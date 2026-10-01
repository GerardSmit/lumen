a = b = c = 5
print(a, b, c)
x = y = []
x.append(1)
print(y)
a, b = 1, 2
a, b = b, a
print(a, b)
a, b, c = "xyz"
print(a, b, c)
first, *rest = [1, 2, 3, 4]
print(first, rest)
*init, last = (1, 2, 3)
print(init, last)
h, *m, t = range(5)
print(h, m, t)
(p, q), r = (1, 2), 3
print(p, q, r)
[u, v] = [10, 20]
print(u, v)
i = 10
i += 5; print(i)
i -= 3; print(i)
i *= 2; print(i)
i //= 5; print(i)
i %= 3; print(i)
i **= 4; print(i)
i <<= 2; print(i)
i >>= 1; print(i)
i |= 1; print(i)
i &= 6; print(i)
i ^= 5; print(i)
f = 7
f /= 2
print(f)
s = "ab"
s += "cd"
s *= 2
print(s)
l = [1]
l += [2, 3]
l *= 2
print(l)
l2 = l
l2 += [9]
print(l is l2, len(l))
t = (1,)
t2 = t
t += (2,)
print(t, t2)
d = {"a": 1}
d["a"] += 10
d["b"] = d.get("b", 0) + 1
print(d)
lst = [0, 0, 0]
lst[1] += 5
lst[-1] -= 1
lst[0:2] = [7, 8, 9]
print(lst)
class O:
    n = 0
o = O()
o.n += 1
o.m = 5
o.m *= 3
print(o.n, O.n, o.m)
n = 5
print(n if n > 3 else 0, not n, n and 0, n or 0, None or "default", 0 or None)
print(1 < n < 10, 1 < n < 3, 1 == 1.0 != 2, n is not None)
print(-n, +n, ~n, -(-n), not not n, 2 ** 3 ** 2, -2 ** 2, 7 - 3 - 2, 2 * 3 + 4 * 5)
