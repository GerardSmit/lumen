import os
import signal

parent_calls = []
child_flag = []

os.register_at_fork(
    before=lambda: parent_calls.append("before 1"),
    after_in_parent=lambda: parent_calls.append("parent 1"),
    after_in_child=lambda: child_flag.append(1),
)
os.register_at_fork(before=lambda: parent_calls.append("before 2"), after_in_parent=lambda: parent_calls.append("parent 2"))

try:
    os.register_at_fork()
except TypeError as e:
    print("TypeError", e)
try:
    os.register_at_fork(before=1)
except TypeError as e:
    print("TypeError", e)

pid = os.fork()
if pid == 0:
    os._exit(7 if child_flag else 8)
got, status, usage = os.wait4(pid, 0)
print(got == pid, os.WIFEXITED(status), os.WEXITSTATUS(status), type(usage).__name__)
print(parent_calls)

pid = os.fork()
if pid == 0:
    os._exit(3)
got, status, usage = os.wait3(0)
print(got == pid, os.waitstatus_to_exitcode(status), len(usage))

pid = os.fork()
if pid == 0:
    os.kill(os.getpid(), signal.SIGTERM)
    os._exit(0)
info = os.waitid(os.P_PID, pid, os.WEXITED)
print(type(info).__name__, info.si_pid == pid, info.si_signo == signal.SIGCHLD, info.si_code == os.CLD_KILLED, info.si_status == signal.SIGTERM)

try:
    os.wait4(-1, 0)
except ChildProcessError as e:
    print("ChildProcessError", e.errno)

pid, fd = os.forkpty()
if pid == 0:
    os.write(1, b"from child\n")
    os._exit(0)
data = b""
while True:
    try:
        chunk = os.read(fd, 100)
    except OSError:
        break
    if not chunk:
        break
    data += chunk
os.waitpid(pid, 0)
print(data.replace(b"\r\n", b"\n"))
os.close(fd)
