import types


def f():
    yield 1


async def co():
    pass


class A:
    def m(self):
        pass


print(types.FunctionType is type(f), types.LambdaType is types.FunctionType)
print(types.GeneratorType is type(f()))
print(types.MethodType is type(A().m), types.BuiltinFunctionType is type(len))
print(types.ModuleType.__name__, types.NoneType, types.NotImplementedType)
print(types.EllipsisType, types.CodeType.__name__)
ns = types.SimpleNamespace(a=1, b=2)
ns.c = 3
print(ns, ns.a, ns == types.SimpleNamespace(a=1, b=2, c=3))
m = types.ModuleType("mod")
m.x = 5
print(m.__name__, m.x)
mp = types.MappingProxyType({"k": 1})
print(mp["k"], len(mp), list(mp), "k" in mp)
try:
    mp["z"] = 1
except TypeError:
    print("immutable")
bound = types.MethodType(lambda self, v: v * 2, 10)
print(bound(4))
g = types.new_class("G", (object,), {}, lambda ns: ns.update(v=7))
print(g.v, g.__name__)
print(types.DynamicClassAttribute.__name__)
print(isinstance(list[int], types.GenericAlias), isinstance(int | str, types.UnionType))
print(types.coroutine.__name__)
print(types.prepare_class("X", (), {})[0] is type)
try:
    raise ValueError
except ValueError as e:
    print(isinstance(e.__traceback__, types.TracebackType), isinstance(e.__traceback__.tb_frame, types.FrameType))
