from . import util
from .util import Box
from .sub import deep

print("load sibling", __name__)


def make(v):
    return Box(util.f(deep.deep_value(v)))
