import os
import stat
import tempfile

tmp = tempfile.mkdtemp()
d = os.open(tmp, os.O_RDONLY)

fd = os.open("a", os.O_WRONLY | os.O_CREAT, 0o600, dir_fd=d)
os.write(fd, b"hello")
os.close(fd)
print(os.stat("a", dir_fd=d).st_size, os.lstat("a", dir_fd=d).st_size)
print(os.access("a", os.R_OK, dir_fd=d), os.access("zz", os.R_OK, dir_fd=d))
print(os.access("a", os.R_OK, effective_ids=True), os.access("a", os.R_OK, follow_symlinks=False))
os.mkdir("sub", 0o700, dir_fd=d)
print(stat.S_ISDIR(os.stat("sub", dir_fd=d).st_mode))
os.symlink("a", "ln", dir_fd=d)
print(os.readlink("ln", dir_fd=d), stat.S_ISLNK(os.lstat("ln", dir_fd=d).st_mode))
os.link("a", "hl", src_dir_fd=d, dst_dir_fd=d)
print(os.stat("hl", dir_fd=d).st_nlink)
os.link("ln", "hl2", src_dir_fd=d, dst_dir_fd=d, follow_symlinks=False)
print(stat.S_ISLNK(os.lstat("hl2", dir_fd=d).st_mode))
os.rename("hl", "hl3", src_dir_fd=d, dst_dir_fd=d)
os.replace("hl3", "sub/hl4", src_dir_fd=d, dst_dir_fd=d)
os.chmod("a", 0o640, dir_fd=d)
print(oct(stat.S_IMODE(os.stat("a", dir_fd=d).st_mode)))
os.utime("a", (1000, 2000), dir_fd=d)
print(os.stat("a", dir_fd=d).st_mtime)
os.utime("a", ns=(5, 6), dir_fd=d, follow_symlinks=False)
os.mkfifo("ff", dir_fd=d)
print(stat.S_ISFIFO(os.stat("ff", dir_fd=d).st_mode))
os.chown("a", -1, -1, dir_fd=d)
print(sorted(os.listdir(d)))
with os.scandir(d) as it:
    entries = sorted((e.name, e.path, e.is_dir(), e.is_symlink()) for e in it)
print(entries)
print(sorted(os.listdir(os.open(os.path.join(tmp, "sub"), os.O_RDONLY))))
for call in (
    lambda: os.stat("nope", dir_fd=d),
    lambda: os.open("a", os.O_RDONLY, dir_fd=os.open(os.path.join(tmp, "a"), os.O_RDONLY)),
    lambda: os.stat("a", dir_fd=999),
    lambda: os.unlink("sub", dir_fd=d),
    lambda: os.rmdir("a", dir_fd=d),
):
    try:
        call()
    except OSError as e:
        print(type(e).__name__, e.errno, e.filename)
print(os.stat(os.path.join(tmp, "a"), dir_fd=d).st_size)
for n in ("ln", "hl2", "ff", "a", "sub/hl4"):
    os.unlink(n, dir_fd=d)
os.rmdir("sub", dir_fd=d)
print(os.listdir(d))
os.close(d)
os.rmdir(tmp)
print(sorted(f.__name__ for f in os.supports_dir_fd))
print(sorted(f.__name__ for f in os.supports_fd))
print(sorted(f.__name__ for f in os.supports_follow_symlinks))
print(sorted(f.__name__ for f in os.supports_effective_ids))
