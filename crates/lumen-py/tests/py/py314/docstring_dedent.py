"""
    Module docstring
      with indent.
"""

print(repr(__doc__))


def spam():
    """
        This is a docstring with
          leading whitespace.

        It even has multiple paragraphs!
    """


print(repr(spam.__doc__))


class C:
	"""	First line after a tab.
	Second line indented by a tab.
		Third by two."""

	def m(self):
		"""   leading spaces on the only line   """


print(repr(C.__doc__))
print(repr(C.m.__doc__))


def blank_lines():
    """First.


    Last.
    """


print(repr(blank_lines.__doc__))


def no_margin():
    """a
b
  c"""


print(repr(no_margin.__doc__))
lam = lambda: None
print(lam.__doc__)
