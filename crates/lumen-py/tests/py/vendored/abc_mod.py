import abc


class Shape(abc.ABC):
    @abc.abstractmethod
    def area(self):
        ...

    @property
    @abc.abstractmethod
    def name(self):
        ...

    @classmethod
    @abc.abstractmethod
    def make(cls):
        ...

    def describe(self):
        return "%s with area %s" % (self.name, self.area())


try:
    Shape()
except TypeError as e:
    print("TypeError", "abstract class" in str(e))


class Square(Shape):
    name = "square"

    def area(self):
        return 4

    @classmethod
    def make(cls):
        return cls()


print(Square().describe(), Square.make().area())
print(sorted(Shape.__abstractmethods__), Square.__abstractmethods__)


class Partial(Shape):
    def area(self):
        return 0


try:
    Partial()
except TypeError as e:
    print("TypeError", "abstract class" in str(e))


class Duck:
    def area(self):
        return 1


Shape.register(Duck)
print(isinstance(Duck(), Shape), issubclass(Duck, Shape), issubclass(int, Shape))


class Closeable(abc.ABC):
    @classmethod
    def __subclasshook__(cls, C):
        if cls is Closeable:
            return hasattr(C, "close") or NotImplemented
        return NotImplemented


class File:
    def close(self):
        pass


print(isinstance(File(), Closeable), issubclass(int, Closeable))


class Meta(abc.ABCMeta):
    pass


class WithMeta(metaclass=Meta):
    @abc.abstractmethod
    def f(self):
        ...


try:
    WithMeta()
except TypeError as e:
    print("abstract")
