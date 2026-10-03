import grp
import pwd

root = pwd.getpwuid(0)
print(root.pw_name, root.pw_uid, len(root), type(root).__name__)
print(pwd.getpwnam("root") == root)
print(isinstance(root.pw_dir, str), isinstance(root.pw_shell, str))
print(any(p.pw_name == "root" for p in pwd.getpwall()))
for call in (lambda: pwd.getpwnam("no-such-user-xyz"), lambda: pwd.getpwuid(54321987), lambda: pwd.getpwnam("a\0b")):
    try:
        call()
    except (KeyError, ValueError) as e:
        print(type(e).__name__, e)

g = grp.getgrgid(0)
print(g.gr_gid, len(g), type(g).__name__, isinstance(g.gr_mem, list))
print(grp.getgrnam(g.gr_name) == g)
print(any(x.gr_gid == 0 for x in grp.getgrall()))
for call in (lambda: grp.getgrnam("no-such-group-xyz"), lambda: grp.getgrgid(54321987)):
    try:
        call()
    except KeyError as e:
        print("KeyError", e)
try:
    pwd.getpwuid("0")
except TypeError as e:
    print("TypeError")
