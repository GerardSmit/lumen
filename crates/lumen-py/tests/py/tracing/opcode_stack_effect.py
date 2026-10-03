import _opcode

POP_TOP = 1
BINARY_SUBSCR = 25
FOR_ITER = 93
JUMP_FORWARD = 110
POP_JUMP_IF_FALSE = 114
BUILD_TUPLE = 102
BUILD_SLICE = 133
LOAD_CONST = 100
CALL = 171

print(_opcode.stack_effect(POP_TOP))
print(_opcode.stack_effect(BINARY_SUBSCR))
print(_opcode.stack_effect(LOAD_CONST, 0))
print(_opcode.stack_effect(BUILD_TUPLE, 3))
print(_opcode.stack_effect(BUILD_SLICE, 0), _opcode.stack_effect(BUILD_SLICE, 1), _opcode.stack_effect(BUILD_SLICE, 3))
print(_opcode.stack_effect(CALL, 2))
print(_opcode.stack_effect(FOR_ITER, 0), _opcode.stack_effect(FOR_ITER, 0, jump=True), _opcode.stack_effect(FOR_ITER, 0, jump=False))
print(_opcode.stack_effect(JUMP_FORWARD, 0, jump=True))
print(_opcode.stack_effect(POP_JUMP_IF_FALSE, 0, jump=False))

for args in ((30000,), (BUILD_SLICE,), (POP_TOP, 0)):
    try:
        _opcode.stack_effect(*args)
    except ValueError:
        print("ValueError", args)

print(_opcode.get_specialization_stats() is None or isinstance(_opcode.get_specialization_stats(), dict))
