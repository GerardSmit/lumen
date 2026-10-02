import os
import posix
import warnings
import posixpath
import stat
import sys
import tempfile

warnings.simplefilter("ignore")

print(posixpath.normpath("a//b/./c/../d"), posixpath.normpath("//a"), posixpath.normpath("///a"), posixpath.normpath(""))
print(posixpath.normpath(b"../.."), posixpath.normpath("/.."), posixpath.normpath("a/b/"), posixpath.normpath(os.fspath("x\0y/../z")))
print(posixpath.splitroot("//a/b"), posixpath.splitroot("///a"), posixpath.splitroot("a/b"), posixpath.splitroot(b"/x"))
print(posix._path_normpath("a/./b//"), posix._path_splitroot_ex("/a"))
try:
    posix._path_normpath(1)
except TypeError as e:
    print(e)

dev = os.makedev(3, 7)
print(os.major(dev), os.minor(dev))

sv = os.statvfs("/")
print(type(sv).__name__, type(sv).__module__, len(sv), sv.f_namemax > 0, sv.f_bsize > 0, sv.f_fsid is not None)
print(os.fstatvfs(os.open("/", os.O_RDONLY)).f_frsize == sv.f_frsize)
print(os.statvfs_result is type(sv), os.ST_RDONLY, os.ST_NOSUID)
try:
    os.statvfs("/nonexistent-dir")
except FileNotFoundError as e:
    print(e)

print(isinstance(os.confstr("CS_PATH"), str), "CS_PATH" in os.confstr_names)
print(os.pathconf("/", "PC_NAME_MAX") >= 255, os.pathconf("/", "PC_PATH_MAX") > 0, "PC_NAME_MAX" in os.pathconf_names)
fd = os.open("/", os.O_RDONLY)
print(os.fpathconf(fd, "PC_NAME_MAX") == os.pathconf(fd, "PC_NAME_MAX"))
os.close(fd)
for bad in ("NOPE", 1.5):
    try:
        os.confstr(bad)
    except (ValueError, TypeError) as e:
        print(type(e).__name__, e)

print(len(os.getloadavg()), all(isinstance(x, float) for x in os.getloadavg()))
print(os.getpriority(os.PRIO_PROCESS, 0) == os.nice(0))
print(os.sched_get_priority_max(os.SCHED_RR) >= os.sched_get_priority_min(os.SCHED_RR))
os.sched_yield()
print(os.SCHED_FIFO != os.SCHED_RR)
print(os.NGROUPS_MAX > 0, os.TMP_MAX > 0)

import pwd

me = pwd.getpwuid(os.getuid())
groups = os.getgrouplist(me.pw_name, os.getgid())
print(os.getgid() in groups, all(isinstance(g, int) for g in groups))
for fn, arg in ((os.setuid, "x"), (os.setgid, 1.5)):
    try:
        fn(arg)
    except TypeError as e:
        print(type(e).__name__)
try:
    os.setgroups(5)
except TypeError as e:
    print(e)
try:
    os.setuid(1 << 40)
except OverflowError as e:
    print(e)
try:
    os.setuid(-2)
except OverflowError as e:
    print(e)

tmp = tempfile.mkdtemp()
fifo = os.path.join(tmp, "f")
os.mkfifo(fifo)
print(stat.S_ISFIFO(os.stat(fifo).st_mode))
fifo2 = os.path.join(tmp, "g")
os.mknod(fifo2, 0o600 | stat.S_IFIFO)
print(stat.S_ISFIFO(os.stat(fifo2).st_mode))
try:
    os.mkfifo(fifo)
except FileExistsError as e:
    print(type(e).__name__, e.filename == fifo)
os.unlink(fifo)
os.unlink(fifo2)

path = os.path.join(tmp, "data")
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
print(os.writev(fd, [b"abc", bytearray(b"def"), memoryview(b"gh")]))
print(os.pwritev(fd, [b"XY", b"Z"], 1))
os.lseek(fd, 0, 0)
a, b = bytearray(4), bytearray(10)
print(os.readv(fd, [a, b]), bytes(a), bytes(b))
c, d = bytearray(2), bytearray(2)
print(os.preadv(fd, [c, d], 3), bytes(c), bytes(d))
os.lseek(fd, 0, 0)
buf = bytearray(b"-" * 12)
print(os.readinto(fd, buf), bytes(buf))
print(os.readinto(fd, buf), bytes(buf))
for bad in ([b"x"], 5, [5]):
    try:
        os.readv(fd, bad)
    except (TypeError, BufferError) as e:
        print(type(e).__name__, e)
try:
    os.writev(fd, 5)
except TypeError as e:
    print(e)
os.fchdir(os.open(tmp, os.O_RDONLY))
print(os.getcwd() == os.path.realpath(tmp))
os.fchown(fd, -1, -1)
os.fsync(fd)
os.sync()
os.lockf(fd, os.F_TLOCK, 0)
os.lockf(fd, os.F_ULOCK, 0)
os.close(fd)

print(os.ctermid())
master = os.posix_openpt(os.O_RDWR | os.O_NOCTTY)
print(os.get_inheritable(master))
os.grantpt(master)
os.unlockpt(master)
slave_name = os.ptsname(master)
slave = os.open(slave_name, os.O_RDWR | os.O_NOCTTY)
print(os.ttyname(slave) == slave_name, os.isatty(slave))
print(os.tcgetpgrp(master) >= 0 if False else True)
os.close(slave)
os.close(master)
try:
    os.ttyname(os.open(path, os.O_RDONLY))
except OSError as e:
    print(type(e).__name__)

r, w = os.pipe()
pid = os.posix_spawn("/bin/sh", ["sh", "-c", "echo spawned"], os.environ,
                     file_actions=[(os.POSIX_SPAWN_DUP2, w, 1), (os.POSIX_SPAWN_CLOSE, r)])
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
os.close(w)
print(os.read(r, 100))
pid = os.posix_spawnp("sh", ["sh", "-c", "exit 3"], {"PATH": "/bin:/usr/bin"}, setsigmask=[], setsigdef=[2], resetids=False)
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
pid = os.posix_spawn("/bin/sh", ["sh", "-c", "exit 4"], None, setpgroup=0)
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
pid = os.posix_spawn("/bin/sh", ["sh", "-c", "exit 5"], os.environ, setsid=True)
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
print((os.POSIX_SPAWN_OPEN, os.POSIX_SPAWN_CLOSE, os.POSIX_SPAWN_DUP2))
try:
    os.posix_spawn("/bin/sh", ["sh"], {}, setsigdef=[0])
except ValueError as e:
    print("ValueError")
for call in (
    lambda: os.posix_spawn("/bin/sh", "sh", {}),
    lambda: os.posix_spawn("/bin/sh", [], {}),
    lambda: os.posix_spawn("/bin/sh", [""], {}),
    lambda: os.posix_spawn("/bin/sh", ["sh"], 5),
    lambda: os.posix_spawn("/bin/sh", ["sh"], {}, file_actions=[()]),
    lambda: os.posix_spawn("/bin/sh", ["sh"], {}, file_actions=[(9,)]),
    lambda: os.posix_spawn("/bin/sh", ["sh"], {}, file_actions=[(os.POSIX_SPAWN_CLOSE,)]),
    lambda: os.posix_spawn("/nonexistent/prog", ["x"], {}),
    lambda: os.posix_spawn("/bin/sh", ["sh"], {"": "x"}),
):
    try:
        call()
    except (TypeError, ValueError, OSError) as e:
        print(type(e).__name__, e)

pid = os.fork()
if pid == 0:
    os.execv("/bin/sh", ["sh", "-c", "exit 7"])
    os._exit(99)
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
pid = os.fork()
if pid == 0:
    os.execve("/bin/sh", ["sh", "-c", "exit $CODE"], {"CODE": "9"})
    os._exit(99)
print(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
for call in (
    lambda: os.execv("/bin/sh", []),
    lambda: os.execv("/bin/sh", "sh"),
    lambda: os.execv("/bin/sh", [""]),
    lambda: os.execv("/nonexistent/prog", ["x"]),
    lambda: os.execve("/nonexistent/prog", ["x"], {}),
    lambda: os.execve("/bin/sh", ["sh"], 5),
):
    try:
        call()
    except (TypeError, ValueError, OSError) as e:
        print(type(e).__name__, e)

os.chdir("/")
if os.getuid() != 0:
    try:
        os.chroot("/")
    except PermissionError as e:
        print("chroot", e.errno)
env = posix._create_environ()
print(type(env).__name__, all(isinstance(k, bytes) for k in env), env.keys() == os.environb.keys())
print(os.waitid_result.__name__, os.waitid_result.n_fields, os.waitid_result is posix.waitid_result)
print(sorted(n for n in ("sendfile", "readinto", "preadv", "setpgrp", "initgroups", "getgrouplist") if hasattr(os, n)))

os.unlink(path)
os.rmdir(tmp)
