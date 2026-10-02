import _thread
import signal
import threading
import time

try:
    t = threading.Thread(target=_thread.interrupt_main)
    t.start()
    t.join()
    for _ in range(1000):
        time.sleep(0.01)
    print("not interrupted")
except KeyboardInterrupt:
    print("KeyboardInterrupt")

got = []
signal.signal(signal.SIGUSR1, lambda s, f: got.append((s, threading.current_thread() is threading.main_thread())))
t = threading.Thread(target=lambda: _thread.interrupt_main(signal.SIGUSR1))
t.start()
t.join()
for _ in range(100):
    if got:
        break
    time.sleep(0.01)
print(got == [(int(signal.SIGUSR1), True)])

print(_thread.stack_size())
try:
    _thread.stack_size(1)
except ValueError as e:
    print("ValueError")

started = threading.Event()
def daemon():
    started.set()
    time.sleep(1000)
d = threading.Thread(target=daemon, daemon=True)
d.start()
started.wait(10)
print(d.daemon, d.is_alive())
