import multiprocessing
import os

ctx = multiprocessing.get_context("fork")

lock = ctx.Lock()
print(lock.acquire(), lock.acquire(False))
lock.release()
print(lock.acquire(timeout=0.01))
lock.release()
try:
    lock.release()
except ValueError as e:
    print("ValueError", e)

rlock = ctx.RLock()
print(rlock.acquire(), rlock.acquire())
rlock.release()
rlock.release()
try:
    rlock.release()
except AssertionError as e:
    print("AssertionError", e)

sem = ctx.Semaphore(2)
print(sem.acquire(), sem.acquire(), sem.acquire(False))
sem.release()
print(sem.acquire(False))

bounded = ctx.BoundedSemaphore(1)
bounded.acquire()
bounded.release()
try:
    bounded.release()
except ValueError as e:
    print("ValueError", e)

event = ctx.Event()
print(event.is_set())
event.set()
print(event.wait(0.01))


def child(conn, lk):
    with lk:
        conn.send(os.getpid() != 0)
    conn.close()


parent_conn, child_conn = ctx.Pipe()
p = ctx.Process(target=child, args=(child_conn, ctx.Lock()))
p.start()
print(parent_conn.recv())
p.join()
print(p.exitcode)
