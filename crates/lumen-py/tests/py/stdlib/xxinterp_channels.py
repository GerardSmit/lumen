import _xxinterpchannels as channels
import _xxsubinterpreters as interpreters

cid = channels.create()
print(type(cid).__name__, cid.end)
channels.send(cid, b"abc")
channels.send(cid, "text")
channels.send(cid, 42)
channels.send(cid, None)
print(channels.recv(cid), channels.recv(cid), channels.recv(cid), channels.recv(cid))
try:
    channels.recv(cid)
except channels.ChannelEmptyError:
    print("empty")
print(channels.recv(cid, "dflt"))

for bad in (1.5, [1], object()):
    try:
        channels.send(cid, bad)
    except ValueError:
        print("not shareable")

print(interpreters.is_shareable(b"x"), interpreters.is_shareable(1.5))

interp = interpreters.create()
print(interpreters.get_current() == interpreters.get_main())
interpreters.run_string(interp, "x = 1 + 1")
try:
    interpreters.run_string(interp, "raise ValueError('boom')")
except interpreters.RunFailedError as e:
    print("failed", e)
interpreters.destroy(interp)
channels.destroy(cid)
