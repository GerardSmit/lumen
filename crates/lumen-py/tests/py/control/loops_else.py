for i in range(5):
    if i == 7:
        break
else:
    print("for-else ran", i)

for i in range(5):
    if i == 3:
        break
else:
    print("not printed")
print("after", i)

n = 0
while n < 3:
    n += 1
else:
    print("while-else", n)

n = 0
while True:
    n += 1
    if n > 4:
        break
else:
    print("not printed")
print(n)

for i in []:
    pass
else:
    print("empty else")

def find(items, t):
    for idx, it in enumerate(items):
        if it == t:
            break
    else:
        return -1
    return idx
print(find([4, 5, 6], 5), find([4, 5, 6], 9))

primes = []
for k in range(2, 30):
    for d in range(2, k):
        if k % d == 0:
            break
    else:
        primes.append(k)
print(primes)

total = 0
for i in range(10):
    if i % 2:
        continue
    if i > 7:
        break
    total += i
else:
    print("no")
print(total)
for i in range(3):
    continue
else:
    print("else after continue", i)
for c in "héy":
    print(c, end="|")
print()
for a, b in zip("ab", [1, 2, 3]):
    print(a, b)
for i, (a, b) in enumerate([(1, 2), (3, 4)]):
    print(i, a + b)
for i in range(10, 0, -3):
    print(i, end=" ")
print()
