# PEP 758: except/except* with several types and no parentheses (only without `as`).
def raiser(exc):
    try:
        raise exc
    except ValueError, TypeError:
        return "value-or-type"
    except KeyError, :
        return "key"
    except (OSError, EOFError) as e:
        return "paren " + type(e).__name__


for e in [ValueError(), TypeError(), KeyError(), EOFError()]:
    print(raiser(e))

try:
    raise ExceptionGroup("g", [ValueError(1), TypeError(2), KeyError(3)])
except* ValueError, TypeError:
    print("star caught value/type")
except* KeyError:
    print("star caught key")

import ast
tree = ast.parse("try:\n    pass\nexcept A, B:\n    pass\n")
print(ast.dump(tree.body[0].handlers[0].type))

for src in [
    "try:\n    pass\nexcept A, B as e:\n    pass\n",
    "try:\n    pass\nexcept* A, B as e:\n    pass\n",
]:
    try:
        compile(src, "<s>", "exec")
    except SyntaxError as e:
        print(e.msg, e.lineno, e.offset)
