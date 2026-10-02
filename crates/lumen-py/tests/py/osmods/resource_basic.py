import mmap
import resource

soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
print(type(soft).__name__, type(hard).__name__, soft <= hard)
print(resource.getpagesize() == mmap.PAGESIZE)
ru = resource.getrusage(resource.RUSAGE_SELF)
print(type(ru).__name__, len(ru), isinstance(ru.ru_utime, float), isinstance(ru.ru_maxrss, int))
print(ru.n_fields if hasattr(ru, "n_fields") else None)
print(resource.RLIM_INFINITY == -1 or resource.RLIM_INFINITY > 0)
resource.setrlimit(resource.RLIMIT_NOFILE, (soft, hard))
print(resource.getrlimit(resource.RLIMIT_NOFILE) == (soft, hard))
for call in (lambda: resource.getrlimit(-1), lambda: resource.getrusage(99), lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (1, 2, 3))):
    try:
        call()
    except ValueError as e:
        print("ValueError", e)
print(resource.error is OSError)
