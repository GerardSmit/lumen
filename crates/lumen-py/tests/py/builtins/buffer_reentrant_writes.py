# In-place writes from __index__ while struct reads a buffer are legal; resizes stay blocked.
import struct

ba = bytearray(b"\x01\x02\x03\x04")


class Off:
    def __index__(self):
        ba[0] = 0x7F
        try:
            ba.append(5)
        except BufferError as e:
            print("append:", type(e).__name__, e)
        return 0


print(struct.unpack_from("<B", ba, Off()))
print(ba)
s = struct.Struct("<BB")
print(s.unpack_from(ba, Off()))
ba.append(9)
print(ba)

mv = memoryview(ba)
print(struct.unpack_from("<B", mv, Off()))
mv.release()
print(bytes(ba))
print(struct.pack_into("<B", ba, Off(), 3), ba)
