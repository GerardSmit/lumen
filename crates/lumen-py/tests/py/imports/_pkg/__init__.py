print("init _pkg", __name__)
from . import sub
from .sub import double, TAG
__all__ = ["double", "TAG", "sub", "PKG_CONST"]
PKG_CONST = "pkg-const"
not_exported = "nope"
