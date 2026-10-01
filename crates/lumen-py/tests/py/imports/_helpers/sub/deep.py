from ..util import f, double
from .. import VERSION
from . import LEVEL

print("load deep", __name__)


def deep_value(x):
    return double(f(x))


def info():
    return VERSION, LEVEL, __package__
