ESCAPES = {"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;"}


def escape(text):
    return "".join(ESCAPES.get(c, c) for c in text)


def render_inline(text):
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == "\\" and i + 1 < n:
            out.append(escape(text[i + 1]))
            i += 2
        elif c == "`":
            j = text.find("`", i + 1)
            if j == -1:
                out.append("`")
                i += 1
            else:
                out.append("<code>" + escape(text[i + 1:j]) + "</code>")
                i = j + 1
        elif text.startswith("**", i):
            j = text.find("**", i + 2)
            if j == -1:
                out.append("**")
                i += 2
            else:
                out.append("<strong>" + render_inline(text[i + 2:j]) + "</strong>")
                i = j + 2
        elif c in "*_":
            j = text.find(c, i + 1)
            if j == -1 or j == i + 1:
                out.append(c)
                i += 1
            else:
                out.append("<em>" + render_inline(text[i + 1:j]) + "</em>")
                i = j + 1
        elif c == "[":
            close = text.find("]", i)
            if close != -1 and text.startswith("(", close + 1):
                end = text.find(")", close + 2)
                if end != -1:
                    label = render_inline(text[i + 1:close])
                    url = escape(text[close + 2:end])
                    out.append('<a href="%s">%s</a>' % (url, label))
                    i = end + 1
                    continue
            out.append("[")
            i += 1
        else:
            out.append(escape(c))
            i += 1
    return "".join(out)


def heading_level(line):
    level = 0
    while level < len(line) and line[level] == "#":
        level += 1
    if 1 <= level <= 6 and line[level:level + 1] == " ":
        return level
    return 0


def list_marker(line):
    stripped = line.lstrip()
    if stripped[:2] in ("- ", "* ", "+ "):
        return "ul", stripped[2:]
    digits = 0
    while digits < len(stripped) and stripped[digits].isdigit():
        digits += 1
    if digits and stripped[digits:digits + 2] == ". ":
        return "ol", stripped[digits + 2:]
    return None, None


def convert(src):
    lines = src.split("\n")
    out = []
    i = 0
    para = []

    def flush():
        if para:
            out.append("<p>" + render_inline(" ".join(para)) + "</p>")
            del para[:]

    while i < len(lines):
        line = lines[i]
        if line.startswith("```"):
            flush()
            lang = line[3:].strip()
            body = []
            i += 1
            while i < len(lines) and not lines[i].startswith("```"):
                body.append(escape(lines[i]))
                i += 1
            i += 1
            cls = ' class="lang-%s"' % lang if lang else ""
            out.append("<pre><code%s>%s</code></pre>" % (cls, "\n".join(body)))
            continue
        if not line.strip():
            flush()
            i += 1
            continue
        level = heading_level(line)
        if level:
            flush()
            out.append("<h%d>%s</h%d>" % (level, render_inline(line[level + 1:].strip()), level))
            i += 1
            continue
        if line.strip() in ("---", "***", "___"):
            flush()
            out.append("<hr>")
            i += 1
            continue
        if line.startswith(">"):
            flush()
            quote = []
            while i < len(lines) and lines[i].startswith(">"):
                quote.append(lines[i][1:].strip())
                i += 1
            out.append("<blockquote>" + render_inline(" ".join(quote)) + "</blockquote>")
            continue
        kind, _ = list_marker(line)
        if kind:
            flush()
            items = []
            while i < len(lines):
                k, rest = list_marker(lines[i])
                if k != kind:
                    break
                items.append("<li>" + render_inline(rest) + "</li>")
                i += 1
            out.append("<%s>\n%s\n</%s>" % (kind, "\n".join(items), kind))
            continue
        para.append(line.strip())
        i += 1
    flush()
    return "\n".join(out)


DOC = """# Title with *emphasis*

Intro paragraph with **bold**, _italic_, and `code <b>`.
It continues on a second line & has entities < > "quotes".

## Lists

- first item
- second **bold** item
* third item

1. one
2. two
3. three with [a link](http://example.com?a=1&b=2)

### Code

```python
def f(x):
    return x < 3 and y > 2
```

> quoted text
> across lines

---

Escaped \\*star\\* and unmatched * star, trailing **bold.
####### not a heading
#### Level four
"""


def main():
    html = convert(DOC)
    print(html)
    print("-----")
    print(len(html.split("\n")), "lines;", html.count("<li>"), "list items")
    for s in ["plain", "**b**", "*i*", "***both***", "a * b", "`x`", "[x](y)", "[x]", "snake_case_name", "&<>"]:
        print(repr(s), "->", render_inline(s))
    print(convert(""))
    print(repr(convert("\n\n")))


main()
