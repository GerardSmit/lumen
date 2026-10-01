class MatrixError(Exception):
    pass


class Matrix:
    def __init__(self, rows):
        rows = [list(r) for r in rows]
        if not rows or not rows[0]:
            raise MatrixError("empty matrix")
        w = len(rows[0])
        for r in rows:
            if len(r) != w:
                raise MatrixError("ragged rows")
        self.rows = rows
        self.n = len(rows)
        self.m = w

    @classmethod
    def identity(cls, n):
        return cls([[1 if i == j else 0 for j in range(n)] for i in range(n)])

    @classmethod
    def zeros(cls, n, m):
        return cls([[0] * m for _ in range(n)])

    @property
    def shape(self):
        return (self.n, self.m)

    def __getitem__(self, idx):
        if isinstance(idx, tuple):
            i, j = idx
            return self.rows[i][j]
        return list(self.rows[idx])

    def __setitem__(self, idx, value):
        i, j = idx
        self.rows[i][j] = value

    def __eq__(self, other):
        if not isinstance(other, Matrix):
            return NotImplemented
        return self.rows == other.rows

    def __hash__(self):
        return hash(tuple(tuple(r) for r in self.rows))

    def __neg__(self):
        return Matrix([[-x for x in r] for r in self.rows])

    def _check_same(self, other):
        if self.shape != other.shape:
            raise MatrixError("shape mismatch %s vs %s" % (self.shape, other.shape))

    def __add__(self, other):
        if not isinstance(other, Matrix):
            return NotImplemented
        self._check_same(other)
        return Matrix([[a + b for a, b in zip(r, s)] for r, s in zip(self.rows, other.rows)])

    def __sub__(self, other):
        if not isinstance(other, Matrix):
            return NotImplemented
        self._check_same(other)
        return Matrix([[a - b for a, b in zip(r, s)] for r, s in zip(self.rows, other.rows)])

    def __mul__(self, other):
        if isinstance(other, Matrix):
            self._check_same(other)
            return Matrix([[a * b for a, b in zip(r, s)] for r, s in zip(self.rows, other.rows)])
        if isinstance(other, (int, float)):
            return Matrix([[x * other for x in r] for r in self.rows])
        return NotImplemented

    def __rmul__(self, other):
        if isinstance(other, (int, float)):
            return self * other
        return NotImplemented

    def __matmul__(self, other):
        if not isinstance(other, Matrix):
            return NotImplemented
        if self.m != other.n:
            raise MatrixError("cannot multiply %s by %s" % (self.shape, other.shape))
        cols = other.transpose().rows
        return Matrix([[sum(a * b for a, b in zip(r, c)) for c in cols] for r in self.rows])

    def __pow__(self, k):
        if not isinstance(k, int) or k < 0:
            raise MatrixError("exponent must be a non-negative int")
        if self.n != self.m:
            raise MatrixError("not square")
        result = Matrix.identity(self.n)
        base = self
        while k:
            if k & 1:
                result = result @ base
            base = base @ base
            k >>= 1
        return result

    def transpose(self):
        return Matrix([list(c) for c in zip(*self.rows)])

    def minor(self, i, j):
        return Matrix([r[:j] + r[j + 1:] for k, r in enumerate(self.rows) if k != i])

    def det(self):
        if self.n != self.m:
            raise MatrixError("not square")
        n = self.n
        if n == 1:
            return self.rows[0][0]
        if n == 2:
            return self.rows[0][0] * self.rows[1][1] - self.rows[0][1] * self.rows[1][0]
        # Bareiss fraction-free elimination
        a = [r[:] for r in self.rows]
        sign = 1
        prev = 1
        for k in range(n - 1):
            if a[k][k] == 0:
                swap = None
                for i in range(k + 1, n):
                    if a[i][k] != 0:
                        swap = i
                        break
                if swap is None:
                    return 0
                a[k], a[swap] = a[swap], a[k]
                sign = -sign
            for i in range(k + 1, n):
                for j in range(k + 1, n):
                    a[i][j] = (a[i][j] * a[k][k] - a[i][k] * a[k][j]) // prev
            prev = a[k][k]
        return sign * a[n - 1][n - 1]

    def det_laplace(self):
        if self.n == 1:
            return self.rows[0][0]
        total = 0
        for j in range(self.m):
            total += (-1) ** j * self.rows[0][j] * self.minor(0, j).det_laplace()
        return total

    def trace(self):
        return sum(self.rows[i][i] for i in range(min(self.shape)))

    def __iter__(self):
        for r in self.rows:
            yield tuple(r)

    def __repr__(self):
        return "Matrix(%r)" % (self.rows,)

    def __str__(self):
        width = max(len(str(x)) for r in self.rows for x in r)
        return "\n".join("[" + " ".join(str(x).rjust(width) for x in r) + "]" for r in self.rows)


def main():
    a = Matrix([[1, 2], [3, 4]])
    b = Matrix([[0, 1], [1, 0]])
    print(repr(a))
    print(a + b)
    print(a - b)
    print(a * b)
    print(a @ b)
    print(2 * a)
    print(a * 3)
    print(-a)
    print(a == Matrix([[1, 2], [3, 4]]), a == b, a == 5, a != b)
    print(a[0, 1], a[1, 0], a[1])
    a[0, 0] = 10
    print(a, a.shape)
    a[0, 0] = 1
    print(a.transpose())
    print(Matrix.identity(3))
    print(Matrix([[1, 2, 3], [4, 5, 6]]).transpose().shape)
    print(a @ Matrix.identity(2) == a)

    fibm = Matrix([[1, 1], [1, 0]])
    print(fibm ** 10)
    print((fibm ** 90)[0, 1])
    print((fibm ** 0) == Matrix.identity(2))

    m3 = Matrix([[2, -3, 1], [2, 0, -1], [1, 4, 5]])
    print(m3.det(), m3.det_laplace())
    m4 = Matrix([[1, 0, 2, -1], [3, 0, 0, 5], [2, 1, 4, -3], [1, 0, 5, 0]])
    print(m4.det(), m4.det_laplace())
    singular = Matrix([[1, 2, 3], [4, 5, 6], [7, 8, 9]])
    print(singular.det())
    pivot = Matrix([[0, 2, 1], [1, 0, 0], [0, 0, 3]])
    print(pivot.det(), pivot.det_laplace())
    print(Matrix([[7]]).det(), a.det(), a.trace())

    h = Matrix([[(i + 1) * (j + 2) % 7 + (i == j) * 5 for j in range(5)] for i in range(5)])
    print(h)
    print(h.det(), h.det_laplace())
    print((h @ h).det() == h.det() ** 2)

    for bad in [
        lambda: a + Matrix([[1, 2, 3]]),
        lambda: Matrix([[1, 2], [3]]),
        lambda: Matrix([[1, 2, 3]]) @ Matrix([[1, 2, 3]]),
        lambda: Matrix([[1, 2, 3]]).det(),
        lambda: a ** -1,
        lambda: a + 1,
        lambda: a @ 2,
    ]:
        try:
            bad()
        except MatrixError as e:
            print("MatrixError:", e)
        except TypeError as e:
            print("TypeError")

    print(list(a), sum(1 for _ in a))
    print({a: "x"}[Matrix([[1, 2], [3, 4]])])
    print(str(Matrix([[1.5, 2], [-3, 4]])))


main()
