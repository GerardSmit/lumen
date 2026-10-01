def level3(n):
    print("level3", n)
    return 10 // n

def level2(n):
    print("level2", n)
    return level3(n - 1) + 1

def level1(n):
    print("level1", n)
    return level2(n - 1) + 1

print(level1(5))
print(level1(2))
print("unreachable")
