import _crypt

print(_crypt.crypt("Hello world!", "$5$saltstring"))
print(_crypt.crypt("Hello world!", "$6$saltstring"))
print(_crypt.crypt("password", "$1$saltsalt$").startswith("$1$saltsalt$"))
print(_crypt.crypt("pw", "$5$rounds=1000$abcdefgh").startswith("$5$rounds=1000$abcdefgh$"))
try:
    _crypt.crypt("a\0b", "$5$salt")
except ValueError as e:
    print("ValueError", e)

import crypt

print(crypt.crypt("Hello world!", "$5$saltstring") == _crypt.crypt("Hello world!", "$5$saltstring"))
print(crypt.METHOD_SHA512.ident, crypt.METHOD_SHA256.ident, crypt.METHOD_MD5.ident)
salt = crypt.mksalt(crypt.METHOD_SHA512)
print(salt.startswith("$6$"), len(salt))
hashed = crypt.crypt("secret", salt)
print(crypt.crypt("secret", hashed) == hashed)
