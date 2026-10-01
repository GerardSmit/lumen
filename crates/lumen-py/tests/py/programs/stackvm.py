class VMError(Exception):
    pass


class StackOverflow(VMError):
    pass


OPS = {}


def op(name, code):
    def deco(fn):
        OPS[code] = (name, fn)
        return fn
    return deco


PUSH, POP, ADD, SUB, MUL, DIV, MOD, DUP, SWAP, LOAD, STORE = range(11)
JMP, JZ, JNZ, LT, EQ, PRINT, CALL, RET, HALT, OVER, NEG = range(11, 22)
NAMES = ["PUSH", "POP", "ADD", "SUB", "MUL", "DIV", "MOD", "DUP", "SWAP", "LOAD", "STORE",
         "JMP", "JZ", "JNZ", "LT", "EQ", "PRINT", "CALL", "RET", "HALT", "OVER", "NEG"]
HAS_ARG = {PUSH, LOAD, STORE, JMP, JZ, JNZ, CALL}


class VM:
    def __init__(self, program, max_stack=64, max_steps=100000):
        self.program = program
        self.stack = []
        self.calls = []
        self.vars = {}
        self.pc = 0
        self.output = []
        self.steps = 0
        self.max_stack = max_stack
        self.max_steps = max_steps

    def push(self, v):
        if len(self.stack) >= self.max_stack:
            raise StackOverflow(f"stack overflow at pc={self.pc}")
        self.stack.append(v)

    def pop(self):
        if not self.stack:
            raise VMError(f"stack underflow at pc={self.pc}")
        return self.stack.pop()

    def run(self):
        prog = self.program
        while True:
            if self.pc >= len(prog):
                raise VMError("ran off end of program")
            self.steps += 1
            if self.steps > self.max_steps:
                raise VMError("step limit exceeded")
            code = prog[self.pc]
            self.pc += 1
            if code in HAS_ARG:
                arg = prog[self.pc]
                self.pc += 1
            if code == PUSH:
                self.push(arg)
            elif code == POP:
                self.pop()
            elif code in (ADD, SUB, MUL, DIV, MOD, LT, EQ):
                b, a = self.pop(), self.pop()
                if code == ADD:
                    self.push(a + b)
                elif code == SUB:
                    self.push(a - b)
                elif code == MUL:
                    self.push(a * b)
                elif code == LT:
                    self.push(int(a < b))
                elif code == EQ:
                    self.push(int(a == b))
                else:
                    if b == 0:
                        raise VMError(f"division by zero at pc={self.pc - 1}")
                    self.push(a // b if code == DIV else a % b)
            elif code == NEG:
                self.push(-self.pop())
            elif code == DUP:
                v = self.pop()
                self.push(v)
                self.push(v)
            elif code == OVER:
                b, a = self.pop(), self.pop()
                self.push(a)
                self.push(b)
                self.push(a)
            elif code == SWAP:
                b, a = self.pop(), self.pop()
                self.push(b)
                self.push(a)
            elif code == LOAD:
                if arg not in self.vars:
                    raise VMError(f"undefined variable {arg}")
                self.push(self.vars[arg])
            elif code == STORE:
                self.vars[arg] = self.pop()
            elif code == JMP:
                self.pc = arg
            elif code == JZ:
                if self.pop() == 0:
                    self.pc = arg
            elif code == JNZ:
                if self.pop() != 0:
                    self.pc = arg
            elif code == PRINT:
                self.output.append(self.pop())
            elif code == CALL:
                self.calls.append(self.pc)
                if len(self.calls) > 50:
                    raise StackOverflow("call depth exceeded")
                self.pc = arg
            elif code == RET:
                if not self.calls:
                    raise VMError("return with empty call stack")
                self.pc = self.calls.pop()
            elif code == HALT:
                return self.output
            else:
                raise VMError(f"bad opcode {code} at {self.pc - 1}")


def assemble(src):
    labels = {}
    items = []
    for line in src.strip().splitlines():
        line = line.split("#")[0].strip()
        if not line:
            continue
        if line.endswith(":"):
            labels[line[:-1]] = sum(2 if t[0] in HAS_ARG else 1 for t in items)
            continue
        parts = line.split()
        name = parts[0].upper()
        if name not in NAMES:
            raise VMError(f"unknown mnemonic {name}")
        items.append((NAMES.index(name), parts[1] if len(parts) > 1 else None))
    code = []
    for opc, arg in items:
        code.append(opc)
        if opc in HAS_ARG:
            if arg is None:
                raise VMError(f"{NAMES[opc]} needs an argument")
            code.append(labels[arg] if arg in labels else (int(arg) if arg.lstrip("-").isdigit() else arg))
    return code


def disassemble(code):
    out = []
    i = 0
    while i < len(code):
        name = NAMES[code[i]]
        if code[i] in HAS_ARG:
            out.append(f"{i:03d} {name} {code[i + 1]}")
            i += 2
        else:
            out.append(f"{i:03d} {name}")
            i += 1
    return out


factorial_src = """
    push 1
    store acc
    push 10
    store n
loop:
    load n
    jz done
    load acc
    load n
    mul
    store acc
    load n
    push 1
    sub
    store n
    jmp loop
done:
    load acc
    print
    halt
"""
fib_src = """
    push 0
    store a
    push 1
    store b
    push 15
    store i
again:
    load i
    jz end
    load a
    print
    load a
    load b
    dup
    store a
    add
    store b
    load i
    push 1
    sub
    store i
    jmp again
end:
    halt
"""
call_src = """
    push 5
    call square
    print
    push 12
    call square
    print
    halt
square:
    dup
    mul
    ret
"""
primes_src = """
    push 2
    store n
outer:
    load n
    push 40
    lt
    jz fin
    push 2
    store d
inner:
    load d
    dup
    mul
    load n
    swap
    lt
    jnz isprime
    load n
    load d
    mod
    jz next
    load d
    push 1
    add
    store d
    jmp inner
isprime:
    load n
    print
next:
    load n
    push 1
    add
    store n
    jmp outer
fin:
    halt
"""

for name, src in [("factorial", factorial_src), ("fib", fib_src), ("call", call_src), ("primes", primes_src)]:
    code = assemble(src)
    vm = VM(code)
    out = vm.run()
    print(f"{name}: len={len(code)} steps={vm.steps} out={out}")
print("\n".join(disassemble(assemble(call_src))))

failures = {
    "underflow": "add\nhalt",
    "divzero": "push 1\npush 0\ndiv\nhalt",
    "undefined": "load x\nhalt",
    "offend": "push 1",
    "overflow": "l:\npush 1\njmp l",
    "recurse": "r:\ncall r",
    "badret": "ret",
    "badop": None,
    "badmnemonic": "frobnicate",
    "noarg": "push",
    "inf": "l:\njmp l",
}
for name, src in failures.items():
    try:
        vm = VM(assemble(src) if src is not None else [99], max_steps=500)
        vm.run()
        print(name, "no error")
    except StackOverflow as ex:
        print(name, "StackOverflow:", ex)
    except VMError as ex:
        print(name, "VMError:", ex)

vm = VM(assemble("push 7\npush 3\nover\nprint\nswap\nneg\nprint\nprint\nhalt"))
print(vm.run(), vm.stack)
print(len(OPS), len(NAMES))
