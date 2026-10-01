class BankError(Exception):
    pass


class InsufficientFunds(BankError):
    def __init__(self, account, needed, available):
        super().__init__(f"{account}: need {needed} cents, have {available}")
        self.account = account
        self.needed = needed
        self.available = available


class AccountFrozen(BankError):
    pass


class InvalidAmount(BankError, ValueError):
    pass


def fmt(cents):
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    return f"{sign}${cents // 100:,}.{cents % 100:02d}"


class Transaction:
    _next = 1

    def __init__(self, kind, amount, balance_after, memo=""):
        self.id = Transaction._next
        Transaction._next += 1
        self.kind = kind
        self.amount = amount
        self.balance_after = balance_after
        self.memo = memo

    def __str__(self):
        return f"#{self.id:03d} {self.kind:<9}{fmt(self.amount):>12}{fmt(self.balance_after):>14}  {self.memo}"


class Account:
    interest_rate_bp = 0

    def __init__(self, owner, balance=0):
        self.owner = owner
        self._balance = 0
        self.frozen = False
        self.history = []
        if balance:
            self.deposit(balance, "opening")

    @property
    def balance(self):
        return self._balance

    def _check(self, amount):
        if not isinstance(amount, int) or isinstance(amount, bool):
            raise InvalidAmount(f"amount must be int cents, got {amount!r}")
        if amount <= 0:
            raise InvalidAmount(f"amount must be positive, got {amount}")
        if self.frozen:
            raise AccountFrozen(f"{self.owner} is frozen")

    def deposit(self, amount, memo=""):
        self._check(amount)
        self._balance += amount
        self.history.append(Transaction("deposit", amount, self._balance, memo))
        return self._balance

    def withdraw(self, amount, memo=""):
        self._check(amount)
        if amount > self.available():
            raise InsufficientFunds(self.owner, amount, self.available())
        self._balance -= amount
        self.history.append(Transaction("withdraw", amount, self._balance, memo))
        return self._balance

    def available(self):
        return self._balance

    def month_end(self):
        interest = self._balance * self.interest_rate_bp // 10000
        if interest > 0:
            self.deposit(interest, "interest")
        return interest

    def __repr__(self):
        return f"{type(self).__name__}({self.owner!r}, {fmt(self._balance)})"

    def __lt__(self, other):
        return self._balance < other._balance


class Savings(Account):
    interest_rate_bp = 250

    def withdraw(self, amount, memo=""):
        if sum(1 for t in self.history if t.kind == "withdraw") >= 3:
            raise BankError(f"{self.owner}: withdrawal limit reached")
        return super().withdraw(amount, memo)


class Checking(Account):
    def __init__(self, owner, balance=0, overdraft=0):
        self.overdraft = overdraft
        super().__init__(owner, balance)

    def available(self):
        return self._balance + self.overdraft


class Bank:
    def __init__(self):
        self.accounts = {}
        self.log = []

    def open(self, kind, owner, *args, **kw):
        if owner in self.accounts:
            raise BankError(f"duplicate account {owner}")
        acct = kind(owner, *args, **kw)
        self.accounts[owner] = acct
        return acct

    def transfer(self, src, dst, amount):
        a, b = self.accounts[src], self.accounts[dst]
        snapshot = (a._balance, b._balance, len(a.history), len(b.history))
        try:
            a.withdraw(amount, f"to {dst}")
            b.deposit(amount, f"from {src}")
        except BankError as ex:
            a._balance, b._balance = snapshot[0], snapshot[1]
            del a.history[snapshot[2]:]
            del b.history[snapshot[3]:]
            self.log.append(f"FAILED transfer {src}->{dst} {fmt(amount)}: {ex}")
            raise
        else:
            self.log.append(f"ok transfer {src}->{dst} {fmt(amount)}")
        finally:
            self.log.append("transfer attempted")

    def total(self):
        return sum(a.balance for a in self.accounts.values())


bank = Bank()
alice = bank.open(Savings, "alice", 100_000)
bob = bank.open(Checking, "bob", 5_000, overdraft=20_000)
carol = bank.open(Checking, "carol")
print(alice, bob, carol)

steps = [
    lambda: bank.transfer("alice", "bob", 25_000),
    lambda: bank.transfer("bob", "carol", 45_000),
    lambda: bank.transfer("bob", "carol", 10_000),
    lambda: bank.transfer("carol", "alice", 1),
    lambda: alice.deposit(-5),
    lambda: alice.deposit(1.5),
    lambda: alice.deposit(True),
    lambda: bank.open(Savings, "alice"),
    lambda: carol.__setattr__("frozen", True),
    lambda: carol.deposit(100),
    lambda: bank.transfer("alice", "carol", 100),
    lambda: alice.withdraw(1),
    lambda: alice.withdraw(1),
]
for i, step in enumerate(steps):
    try:
        step()
        print(i, "ok")
    except InsufficientFunds as ex:
        print(i, "insufficient:", ex, ex.needed - ex.available)
    except AccountFrozen as ex:
        print(i, "frozen:", ex)
    except InvalidAmount as ex:
        print(i, "invalid:", ex, isinstance(ex, ValueError))
    except BankError as ex:
        print(i, "bank error:", ex)

print("interest", alice.month_end(), bob.month_end(), carol.month_end())
print(sorted(bank.accounts.values()))
print(max(bank.accounts.values()).owner, min(bank.accounts.values()).owner)
for owner in sorted(bank.accounts):
    acct = bank.accounts[owner]
    print("==", owner, fmt(acct.balance), "avail", fmt(acct.available()))
    for t in acct.history:
        print("  ", t)
print("\n".join(bank.log))
print("total", fmt(bank.total()))
print(fmt(-123456), fmt(0), fmt(5), fmt(100000000))
print([c.__name__ for c in InvalidAmount.__mro__])
