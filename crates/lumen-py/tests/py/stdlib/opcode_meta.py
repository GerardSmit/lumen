import _opcode
import opcode


def t(f, *a, **k):
    try:
        print(f.__name__, a, k, '->', repr(f(*a, **k)))
    except Exception as e:
        print(f.__name__, a, k, 'EXC', type(e).__name__, e)


fns = [_opcode.is_valid, _opcode.has_arg, _opcode.has_const, _opcode.has_name,
       _opcode.has_jump, _opcode.has_free, _opcode.has_local, _opcode.has_exc]
for op in list(range(-2, 270)) + [1000]:
    print(op, [int(f(op)) for f in fns])
    for j in (None, True, False):
        for a in (None, 0, 1, 2, 3, 7, 64):
            try:
                r = _opcode.stack_effect(op, a, jump=j)
            except ValueError as e:
                r = 'ValueError'
            print(op, a, j, r)

t(_opcode.stack_effect, 70, 0, jump=1)
t(_opcode.stack_effect, 70, 0, jump='x')
t(_opcode.stack_effect, 44, 1.5)
t(_opcode.stack_effect, 44, 'a')
t(_opcode.stack_effect, 31)
t(_opcode.stack_effect, 84)
t(_opcode.stack_effect, 31, 5)
t(_opcode.has_arg, 1.0)
print(_opcode.get_nb_ops())
print(_opcode.get_intrinsic1_descs())
print(_opcode.get_intrinsic2_descs())
print(_opcode.get_special_method_names())
print(_opcode.get_specialization_stats())
print(sorted(n for n in dir(_opcode) if not n.startswith('__')))
for n in sorted(n for n in dir(_opcode) if not n.startswith("__")):
    if callable(getattr(_opcode, n)):
        print(n, getattr(_opcode, n).__text_signature__)

def f(): pass
t(_opcode.get_executor, 1, 2)
try:
    _opcode.get_executor(f.__code__, 0)
except RuntimeError as e:
    print(e)

for n in ['hasarg', 'hasconst', 'hasname', 'hasjump', 'hasfree', 'haslocal', 'hasexc', 'hascompare']:
    print(n, getattr(opcode, n))
print(opcode.opname)
print(opcode.opmap)
print(opcode._specializations)
print(opcode.HAVE_ARGUMENT, opcode.MIN_INSTRUMENTED_OPCODE, opcode.EXTENDED_ARG)
print(opcode._intrinsic_1_descs, opcode._intrinsic_2_descs, opcode._special_method_names)
print(opcode._nb_ops)
print(opcode._cache_format)
print(opcode._inline_cache_entries)
print(opcode.stack_effect(opcode.opmap['LOAD_ATTR'], 1), opcode.stack_effect(opcode.opmap['FOR_ITER'], 0, jump=False))
