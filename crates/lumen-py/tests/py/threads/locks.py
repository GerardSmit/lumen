import threading
import _thread
import time

lock = _thread.allocate_lock()
print(lock.locked(), lock.acquire(), lock.locked())
print(lock.acquire(False))
print(lock.acquire(True, 0.05))
try:
    lock.acquire(False, 1)
except ValueError as e:
    print(e)
try:
    lock.acquire(timeout=-2)
except ValueError as e:
    print("ValueError")
try:
    lock.acquire(timeout=1e100)
except OverflowError as e:
    print("OverflowError")
lock.release()
try:
    lock.release()
except RuntimeError as e:
    print(e)

lock.acquire()
order = []
def waiter():
    order.append("waiting")
    got = lock.acquire(timeout=10)
    order.append(got)
    lock.release()

t = threading.Thread(target=waiter)
t.start()
while not order:
    time.sleep(0.001)
time.sleep(0.05)
order.append("releasing")
lock.release()
t.join()
print(order)

r = threading.RLock()
with r:
    with r:
        print(r._is_owned(), r._recursion_count())
print(r._is_owned())
try:
    r.release()
except RuntimeError as e:
    print(e)

other = []
def grab():
    other.append(r.acquire(False))

with r:
    t = threading.Thread(target=grab)
    t.start()
    t.join()
print(other)

cond = threading.Condition()
items = []
def producer():
    for i in range(5):
        with cond:
            items.append(i)
            cond.notify()

def consumer(out):
    while len(out) < 5:
        with cond:
            while not items:
                cond.wait()
            out.append(items.pop(0))

out = []
c = threading.Thread(target=consumer, args=(out,))
c.start()
p = threading.Thread(target=producer)
p.start()
p.join()
c.join()
print(out)

sem = threading.Semaphore(2)
active = []
peak = [0]
guard = threading.Lock()
def use():
    with sem:
        with guard:
            active.append(1)
            peak[0] = max(peak[0], len(active))
        time.sleep(0.01)
        with guard:
            active.pop()

ts = [threading.Thread(target=use) for _ in range(6)]
for t in ts:
    t.start()
for t in ts:
    t.join()
print(peak[0] <= 2)
print(_thread.TIMEOUT_MAX > 1e9)
