from _testbuffer import ndarray, ND_WRITABLE, ND_FORTRAN, PyBUF_FULL_RO

a = ndarray(list(range(12)), shape=[3, 4], flags=ND_WRITABLE)
print(a.shape, a.strides, a.ndim, a.itemsize, a.format, a.readonly)
print(a.tolist())
print(a.c_contiguous, a.f_contiguous)
m = memoryview(a)
print(m.shape, m.strides, m.tolist())

f = ndarray(list(range(6)), shape=[2, 3], flags=ND_FORTRAN)
print(f.strides, f.f_contiguous, f.tolist())

b = ndarray([97, 98, 99, 100], shape=[4], format="B")
print(b.tolist(), b.readonly, b.nbytes)
try:
    b[0] = 1
except TypeError:
    print("TypeError")
print(PyBUF_FULL_RO)
