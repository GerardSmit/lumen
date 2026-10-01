class MinHeap:
    def __init__(self, key=None):
        self._a = []
        self._key = key or (lambda x: x)

    def __len__(self):
        return len(self._a)

    def __bool__(self):
        return bool(self._a)

    def peek(self):
        if not self._a:
            raise IndexError("peek from empty heap")
        return self._a[0]

    def push(self, item):
        self._a.append(item)
        self._sift_up(len(self._a) - 1)

    def pop(self):
        a = self._a
        if not a:
            raise IndexError("pop from empty heap")
        top = a[0]
        last = a.pop()
        if a:
            a[0] = last
            self._sift_down(0)
        return top

    def _less(self, i, j):
        return self._key(self._a[i]) < self._key(self._a[j])

    def _sift_up(self, i):
        a = self._a
        while i > 0:
            parent = (i - 1) // 2
            if self._less(i, parent):
                a[i], a[parent] = a[parent], a[i]
                i = parent
            else:
                break

    def _sift_down(self, i):
        a = self._a
        n = len(a)
        while True:
            l, r = 2 * i + 1, 2 * i + 2
            smallest = i
            if l < n and self._less(l, smallest):
                smallest = l
            if r < n and self._less(r, smallest):
                smallest = r
            if smallest == i:
                return
            a[i], a[smallest] = a[smallest], a[i]
            i = smallest

    def is_valid(self):
        for i in range(1, len(self._a)):
            if self._less(i, (i - 1) // 2):
                return False
        return True


class PriorityQueue:
    def __init__(self):
        self._heap = MinHeap()
        self._counter = 0

    def push(self, priority, item):
        self._counter += 1
        self._heap.push((priority, self._counter, item))

    def pop(self):
        priority, _, item = self._heap.pop()
        return priority, item

    def __len__(self):
        return len(self._heap)


class Task:
    def __init__(self, name, priority, duration, arrival=0):
        self.name = name
        self.priority = priority
        self.duration = duration
        self.remaining = duration
        self.arrival = arrival
        self.finished_at = None

    def __repr__(self):
        return "Task(%s,p=%d,d=%d)" % (self.name, self.priority, self.duration)


def schedule(tasks, quantum):
    pending = sorted(tasks, key=lambda t: (t.arrival, t.name))
    pq = PriorityQueue()
    clock = 0
    timeline = []
    idx = 0
    while idx < len(pending) or len(pq):
        while idx < len(pending) and pending[idx].arrival <= clock:
            pq.push(pending[idx].priority, pending[idx])
            idx += 1
        if not len(pq):
            clock = pending[idx].arrival
            continue
        _, task = pq.pop()
        run = min(quantum, task.remaining)
        timeline.append((clock, task.name, run))
        clock += run
        task.remaining -= run
        while idx < len(pending) and pending[idx].arrival <= clock:
            pq.push(pending[idx].priority, pending[idx])
            idx += 1
        if task.remaining:
            pq.push(task.priority, task)
        else:
            task.finished_at = clock
    return timeline


def heap_sort(values):
    h = MinHeap()
    for v in values:
        h.push(v)
    return [h.pop() for _ in range(len(values))]


def main():
    seed = 7
    nums = []
    for _ in range(40):
        seed = (seed * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        nums.append((seed >> 33) % 1000)
    print(nums[:10])
    srt = heap_sort(nums)
    print(srt == sorted(nums), srt[:8], srt[-3:])

    h = MinHeap(key=lambda p: -p)
    for v in [5, 1, 9, 3, 7]:
        h.push(v)
    print(h.peek(), h.is_valid(), [h.pop() for _ in range(len(h))], bool(h))
    try:
        h.pop()
    except IndexError as e:
        print("IndexError:", e)

    pq = PriorityQueue()
    for pri, name in [(2, "b"), (1, "a"), (2, "c"), (1, "d"), (3, "e"), (2, "f"), (1, "g")]:
        pq.push(pri, name)
    order = []
    while len(pq):
        order.append(pq.pop())
    print(order)

    tasks = [
        Task("compile", 2, 5, 0),
        Task("test", 3, 4, 1),
        Task("lint", 1, 2, 2),
        Task("deploy", 4, 3, 3),
        Task("docs", 3, 6, 4),
        Task("late", 1, 1, 40),
    ]
    tl = schedule(tasks, 2)
    for start, name, run in tl:
        print("t=%2d %-8s +%d" % (start, name, run))
    for t in sorted(tasks, key=lambda t: (t.finished_at, t.name)):
        print("%-8s done at %2d (turnaround %d)" % (t.name, t.finished_at, t.finished_at - t.arrival))
    print("total busy:", sum(r for _, _, r in tl), "switches:", len(tl))

    big = MinHeap()
    for i in range(1000):
        big.push((i * 7919) % 1009)
    print(big.is_valid(), big.peek(), len(big))
    popped = [big.pop() for _ in range(5)]
    print(popped, big.is_valid())


main()
