import _testmultiphase as t

print(t.__name__)
print(t.Example.__name__)
e = t.Example()
print(e.demo(5))
print(t.int_const, t.str_const)
