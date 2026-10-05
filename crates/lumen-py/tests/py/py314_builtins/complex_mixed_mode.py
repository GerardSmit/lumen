import math
def t(f):
    try: print(repr(f()))
    except BaseException as e: print(type(e).__name__, e)
inf=float('inf'); nan=float('nan')
vals=[0.0,-0.0,1.0,-2.5,inf,-inf,nan]
cs=[complex(a,b) for a in (0.0,-0.0,1.0,inf,nan) for b in (0.0,-0.0,2.0,-inf,nan)]
import operator as o
for op in (o.add,o.sub,o.mul,o.truediv):
  for f in vals:
    for c in cs:
      t(lambda: op(f,c)); t(lambda: op(c,f))
for f in (1,-3,0,10**30):
    for c in cs:
      for op in (o.add,o.sub,o.mul,o.truediv):
        t(lambda: op(f,c)); t(lambda: op(c,f))
