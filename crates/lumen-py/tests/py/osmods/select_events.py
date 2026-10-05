import os
import select

r, w = os.pipe()

if hasattr(select, "epoll"):
    ep = select.epoll()
    print(ep.closed, ep.fileno() >= 0)
    ep.register(r, select.EPOLLIN)
    print(ep.poll(0))
    os.write(w, b"x")
    print(ep.poll(1) == [(r, select.EPOLLIN)])
    ep.modify(r, select.EPOLLIN | select.EPOLLET)
    ep.unregister(r)
    try:
        ep.unregister(r)
    except OSError as e:
        print("OSError", e.errno)
    try:
        ep.poll(0, 0)
    except ValueError as e:
        print("ValueError", e)
    with ep:
        pass
    print(ep.closed)
    try:
        ep.fileno()
    except ValueError as e:
        print("ValueError", e)
    print(hasattr(select, "kqueue"))
else:
    kq = select.kqueue()
    print(kq.closed, kq.fileno() >= 0)
    ev = select.kevent(r, select.KQ_FILTER_READ, select.KQ_EV_ADD | select.KQ_EV_ENABLE)
    print(ev.ident == r, ev.filter == select.KQ_FILTER_READ, ev.flags == select.KQ_EV_ADD | select.KQ_EV_ENABLE)
    print(kq.control([ev], 1, 0))
    os.write(w, b"x")
    out = kq.control(None, 1, 1)
    print(len(out), out[0].ident == r, out[0].filter == select.KQ_FILTER_READ, out[0].data)
    print(select.kevent(1) == select.kevent(1), select.kevent(1) < select.kevent(2))
    try:
        kq.control(None, -1)
    except ValueError as e:
        print("ValueError", e)
    try:
        kq.control(5, 1)
    except TypeError as e:
        print("TypeError", e)
    kq.close()
    print(kq.closed)
    try:
        kq.fileno()
    except ValueError as e:
        print("ValueError", e)
    print(hasattr(select, "epoll"))

p = select.poll()
p.register(r, select.POLLIN)
print(p.poll(0) == [(r, select.POLLIN)])
os.close(r)
os.close(w)
