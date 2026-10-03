import threading
import _thread

results = []
lock = threading.Lock()

def worker(n):
    with lock:
        results.append(n * n)

threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
for t in threads:
    t.start()
for t in threads:
    t.join()
print(sorted(results))
print(threading.active_count())

done = threading.Event()
ident = []
def record():
    ident.append(threading.get_ident())
    done.set()

t = threading.Thread(target=record)
t.start()
done.wait(10)
t.join()
print(ident[0] != threading.get_ident(), t.is_alive())

box = []
t = _thread.start_new_thread(lambda a, b=0: box.append(a + b), (1,), {"b": 2})
while not box:
    pass
print(box, isinstance(t, int))

class Failing(threading.Thread):
    def run(self):
        raise ValueError("boom")

caught = []
threading.excepthook = lambda args: caught.append((args.exc_type.__name__, str(args.exc_value)))
f = Failing()
f.start()
f.join()
print(caught)

try:
    _thread.start_new_thread(1, ())
except TypeError as e:
    print(e)
try:
    _thread.start_new_thread(print, [])
except TypeError as e:
    print(e)
