import os
import stat
import sys
import tempfile
import time

tmp = tempfile.mkdtemp()
p = os.path.join(tmp, "f")
with open(p, "wb") as f:
    f.write(b"abc")
st = os.stat(p)
print(hasattr(st, "st_birthtime"), hasattr(st, "st_birthtime_ns"), hasattr(st, "st_flags"), hasattr(st, "st_gen"))
print(type(st).__name__, st.n_sequence_fields, st.n_fields, st.n_unnamed_fields)
print(sorted(n for n in dir(st) if n.startswith("st_")))
print(os.process_cpu_count() is not None, os.cpu_count() >= 1)
print(os.get_terminal_size.__name__, os.terminal_size.__name__, os.terminal_size((80, 24)))
print(os.write.__text_signature__, os.ftruncate.__text_signature__)
print(os.chmod in os.supports_follow_symlinks, os.chmod in os.supports_fd, os.chown in os.supports_dir_fd)
print(sorted(f.__name__ for f in os.supports_fd))
print(sorted(f.__name__ for f in os.supports_dir_fd))
print(sorted(f.__name__ for f in os.supports_follow_symlinks))
print(sorted(f.__name__ for f in os.supports_effective_ids))
print(os.chmod(p, 0o600), stat.S_IMODE(os.stat(p).st_mode))
fd = os.open(p, os.O_RDWR)
os.fchmod(fd, 0o640)
print(oct(stat.S_IMODE(os.fstat(fd).st_mode)))
os.ftruncate(fd, 1)
print(os.fstat(fd).st_size)
print(os.write(fd, b"zz"), os.pread(fd, 10, 0), os.pwrite(fd, b"Q", 0))
os.close(fd)
print(os.listdrives if hasattr(os, "listdrives") else "nodrives")
print(os.get_blocking(0) in (True, False), os.sched_getaffinity if hasattr(os, "sched_getaffinity") else "no-affinity")
print(os.EX_OK, os.EX_USAGE, os.EX_CONFIG, os.P_NOWAIT, os.WNOHANG, os.WEXITED, os.P_PID, os.P_ALL, os.P_PGID)
print(os.name, os.sep, os.linesep == "\n", os.devnull, os.curdir, os.pardir, os.extsep, os.altsep, os.pathsep)
print(os.supports_bytes_environ, os.getenv("PATH") is not None, os.getenvb(b"PATH") is not None)
print(sorted(os.sysconf_names)[:5], os.sysconf("SC_PAGE_SIZE") == os.sysconf("SC_PAGESIZE"))
print(os.times().elapsed > 0, len(os.uname()), os.uname().n_fields)
print(os.strerror(2), os.getppid() > 0, os.umask(0o22) >= 0)
print(os.path.splitroot("/a/b"), os.path.normpath("/a/../b"))
for name in ("lchmod", "chflags", "lchflags", "O_EXLOCK", "O_SHLOCK", "O_EVTONLY", "O_NOFOLLOW_ANY", "O_SYMLINK", "O_EXEC", "O_SEARCH", "O_FSYNC", "O_DSYNC", "O_RSYNC", "O_TTY_INIT", "O_PATH"):
    print(name, hasattr(os, name))
with os.scandir(tmp) as it:
    e = next(it)
    print(e.name, e.is_file(), e.stat().st_size, e.inode() > 0, e.is_junction())
print(os.fspath(os.DirEntry) if False else os.DirEntry.__name__)
print(time.clock_gettime(time.CLOCK_MONOTONIC) > 0, time.clock_getres(time.CLOCK_REALTIME) > 0)
print(time.get_clock_info("monotonic").implementation, time.get_clock_info("perf_counter").resolution < 1)
print(time.get_clock_info("time").adjustable, time.get_clock_info("process_time").monotonic)
print(time.strftime("%Y", time.gmtime(0)), time.mktime(time.localtime(86400)) == 86400.0)
t = time.gmtime(1e9)
print(t, t.tm_zone, t.tm_gmtoff)
print(time.strptime("2020-02-29", "%Y-%m-%d").tm_yday, time.asctime(t), time.ctime(0)[:3])
print(time.struct_time((2000, 1, 2, 3, 4, 5, 6, 7, 8)).tm_isdst)
try:
    time.sleep(-1)
except ValueError as e:
    print(e)
try:
    time.strftime("%Y", (1,))
except TypeError as e:
    print(e)
os.unlink(p)
os.rmdir(tmp)
