PUSH, POP, ADD, SUB, MUL, DIV, MOD = range(7)
DUP, SWAP, OVER = 7, 8, 9
LOAD, STORE = 10, 11
JMP, JZ, JNZ = 12, 13, 14
CALL, RET = 15, 16
LT, EQ = 17, 18
PRINT, HALT = 19, 20

NAMES = {
    PUSH: "PUSH", POP: "POP", ADD: "ADD", SUB: "SUB", MUL: "MUL", DIV: "DIV", MOD: "MOD",
    DUP: "DUP", SWAP: "SWAP", OVER: "OVER", LOAD: "LOAD", STORE: "STORE",
    JMP: "JMP", JZ: "JZ", JNZ: "JNZ", CALL: "CALL", RET: "RET", LT: "LT", EQ: "EQ",
    PRINT: "PRINT", HALT: "HALT",
}
OPCODES = {v: k for k, v in NAMES.items()}
HAS_ARG = {PUSH, LOAD, STORE, JMP, JZ, JNZ, CALL}


class VMError(Exception):
    pass


def assemble(source):
    labels = {}
    items = []
    for raw in source.strip().split("\n"):
        line = raw.split("#")[0].strip()
        if not line:
            continue
        if line.endswith(":"):
            labels[line[:-1]] = len(items)
            continue
        parts = line.split()
        items.append(parts)
    code = []
    for parts in items:
        op = OPCODES.get(parts[0].upper())
        if op is None:
            raise VMError("unknown mnemonic " + parts[0])
        if op in HAS_ARG:
            if len(parts) != 2:
                raise VMError("%s needs an argument" % parts[0])
            arg = parts[1]
            if arg in labels:
                arg = labels[arg]
            else:
                arg = int(arg)
            code.append((op, arg))
        else:
            code.append((op, None))
    return code


def disassemble(code):
    lines = []
    for i, (op, arg) in enumerate(code):
        text = NAMES[op] if arg is None else "%s %d" % (NAMES[op], arg)
        lines.append("%03d  %s" % (i, text))
    return lines


class VM:
    def __init__(self, code, max_steps=1000000):
        self.code = code
        self.stack = []
        self.calls = []
        self.mem = {}
        self.pc = 0
        self.steps = 0
        self.output = []
        self.max_steps = max_steps
        self.halted = False

    def pop(self):
        if not self.stack:
            raise VMError("stack underflow at pc=%d" % (self.pc - 1))
        return self.stack.pop()

    def step(self):
        if self.pc >= len(self.code):
            raise VMError("pc out of range: %d" % self.pc)
        op, arg = self.code[self.pc]
        self.pc += 1
        self.steps += 1
        st = self.stack
        if op == PUSH:
            st.append(arg)
        elif op == POP:
            self.pop()
        elif op in (ADD, SUB, MUL, DIV, MOD, LT, EQ):
            b = self.pop()
            a = self.pop()
            if op == ADD:
                st.append(a + b)
            elif op == SUB:
                st.append(a - b)
            elif op == MUL:
                st.append(a * b)
            elif op == LT:
                st.append(1 if a < b else 0)
            elif op == EQ:
                st.append(1 if a == b else 0)
            else:
                if b == 0:
                    raise VMError("division by zero at pc=%d" % (self.pc - 1))
                st.append(a // b if op == DIV else a % b)
        elif op == DUP:
            v = self.pop()
            st.extend((v, v))
        elif op == SWAP:
            b = self.pop()
            a = self.pop()
            st.extend((b, a))
        elif op == OVER:
            b = self.pop()
            a = self.pop()
            st.extend((a, b, a))
        elif op == LOAD:
            st.append(self.mem.get(arg, 0))
        elif op == STORE:
            self.mem[arg] = self.pop()
        elif op == JMP:
            self.pc = arg
        elif op == JZ:
            if self.pop() == 0:
                self.pc = arg
        elif op == JNZ:
            if self.pop() != 0:
                self.pc = arg
        elif op == CALL:
            self.calls.append(self.pc)
            self.pc = arg
        elif op == RET:
            if not self.calls:
                raise VMError("return with empty call stack")
            self.pc = self.calls.pop()
        elif op == PRINT:
            self.output.append(self.pop())
        elif op == HALT:
            self.halted = True

    def run(self):
        while not self.halted:
            if self.steps >= self.max_steps:
                raise VMError("step limit exceeded")
            self.step()
        return self.output


FACTORIAL = """
    push 10
    store 0          # n
    push 1
    store 1          # acc
loop:
    load 0
    jz done
    load 1
    load 0
    mul
    store 1
    load 0
    push 1
    sub
    store 0
    jmp loop
done:
    load 1
    print
    halt
"""

SQUARES_WITH_CALL = """
    push 1
    store 0
next:
    load 0
    call square
    print
    load 0
    push 1
    add
    dup
    store 0
    push 8
    lt
    jnz next
    halt
square:
    dup
    mul
    ret
"""

GCD = """
    push 1071
    push 462
again:
    dup
    jz fin
    swap
    over
    mod
    jmp again
fin:
    pop
    print
    halt
"""

PRIMES = """
    push 2
    store 0            # candidate
cand:
    push 2
    store 1            # divisor
trial:
    load 1
    load 1
    mul
    load 0
    swap
    lt                 # candidate < d*d ?
    jnz isprime
    load 0
    load 1
    mod
    jz notprime
    load 1
    push 1
    add
    store 1
    jmp trial
isprime:
    load 0
    print
notprime:
    load 0
    push 1
    add
    dup
    store 0
    push 50
    lt
    jnz cand
    halt
"""


def run(name, src, **kw):
    code = assemble(src)
    vm = VM(code, **kw)
    try:
        out = vm.run()
        print("%-10s ok   steps=%-5d out=%s" % (name, vm.steps, out))
    except VMError as e:
        print("%-10s FAIL steps=%-5d %s" % (name, vm.steps, e))
    return vm


def main():
    print("\n".join(disassemble(assemble(GCD))))
    run("factorial", FACTORIAL)
    run("squares", SQUARES_WITH_CALL)
    run("gcd", GCD)
    vm = run("primes", PRIMES)
    print(sorted(vm.mem.items()))
    run("underflow", "add\nhalt")
    run("divzero", "push 1\npush 0\ndiv\nhalt")
    run("badret", "ret")
    run("runaway", "top:\njmp top", max_steps=500)
    run("falloff", "push 1")
    try:
        assemble("frobnicate")
    except VMError as e:
        print("assemble error:", e)
    try:
        assemble("push")
    except VMError as e:
        print("assemble error:", e)
    big = run("bigmul", "push 2\nstore 0\npush 1\nstore 1\nl:\nload 1\nload 0\nmul\nstore 1\nload 0\npush 1\nadd\nstore 0\nload 0\npush 40\nlt\njnz l\nload 1\nprint\nhalt")
    print(big.mem[1] > 2 ** 100)


main()
