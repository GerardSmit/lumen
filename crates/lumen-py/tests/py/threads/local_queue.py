import sys
import threading
import queue

data = threading.local()
data.x = "main"
seen = []

def worker(n):
    seen.append(hasattr(data, "x"))
    data.x = n
    seen.append(data.x)

for i in range(3):
    t = threading.Thread(target=worker, args=(i,))
    t.start()
    t.join()
print(seen, data.x)

class Init(threading.local):
    def __init__(self, v):
        self.v = v

d = Init(7)
res = []
t = threading.Thread(target=lambda: res.append(d.v))
t.start()
t.join()
print(res, d.v)

q = queue.Queue()
total = []
def consume():
    while True:
        item = q.get()
        if item is None:
            q.task_done()
            return
        total.append(item)
        q.task_done()

cs = [threading.Thread(target=consume) for _ in range(3)]
for c in cs:
    c.start()
for i in range(20):
    q.put(i)
for _ in cs:
    q.put(None)
q.join()
for c in cs:
    c.join()
print(sorted(total))

try:
    q2 = queue.Queue(1)
    q2.put(1)
    q2.put(2, timeout=0.05)
except queue.Full:
    print("full")

ev = threading.Event()
stop = threading.Event()
def idle():
    ev.set()
    stop.wait(10)
t = threading.Thread(target=idle)
t.start()
ev.wait(10)
cf = sys._current_frames()
print(t.ident in cf, threading.get_ident() in cf)
stop.set()
t.join()
