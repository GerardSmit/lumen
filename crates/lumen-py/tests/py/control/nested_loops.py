rows = []
for i in range(1, 4):
    row = []
    for j in range(1, 4):
        row.append(i * j)
    rows.append(row)
print(rows)

for i in range(3):
    for j in range(3):
        if j == 2:
            break
        if i == 1:
            continue
        print(i, j)

found = None
for i in range(1, 20):
    for j in range(i, 20):
        if i * j == 42 and i + j == 13:
            found = (i, j)
            break
    if found:
        break
print(found)

flag = False
for i in range(3):
    for j in range(3):
        if i + j == 3:
            flag = True
            break
    else:
        continue
    break
print(flag, i, j)

i = 0
out = []
while i < 4:
    j = 0
    while j < i:
        out.append((i, j))
        j += 1
    i += 1
print(out)

tri = [[0] * k for k in range(1, 5)]
for r, row in enumerate(tri):
    for c in range(len(row)):
        row[c] = r + c
print(tri)
s = 0
for i in range(100):
    for j in range(100):
        if (i + j) % 7 == 0:
            s += 1
print(s)
print(sum(1 for i in range(5) for j in range(i)))
print([(i, j) for i in range(3) for j in range(3) if i != j])
