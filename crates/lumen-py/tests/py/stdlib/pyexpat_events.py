from xml.parsers import expat

events = []


def rec(name):
    def handler(*args):
        events.append((name,) + args)

    return handler


p = expat.ParserCreate()
p.ordered_attributes = True
for h in ("StartElementHandler", "EndElementHandler", "CharacterDataHandler",
          "ProcessingInstructionHandler", "CommentHandler", "StartCdataSectionHandler",
          "EndCdataSectionHandler", "XmlDeclHandler", "StartDoctypeDeclHandler",
          "EndDoctypeDeclHandler", "ElementDeclHandler", "AttlistDeclHandler",
          "EntityDeclHandler", "NotationDeclHandler"):
    setattr(p, h, rec(h[:-7]))

doc = b"""<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<!DOCTYPE root [
<!ELEMENT root (a|b)*>
<!ELEMENT a (#PCDATA)>
<!ATTLIST root id CDATA "x" kind (p|q) #REQUIRED>
<!ENTITY e "hello">
<!NOTATION n SYSTEM "n.dtd">
]>
<root id="1" kind="p">text &e; <a>A</a><!--c--><?pi data?><![CDATA[<raw>]]></root>
"""
print(p.Parse(doc, True))
for e in events:
    print(e)

# buffered character data arrives in one piece
parts = []
p = expat.ParserCreate()
p.buffer_text = True
p.CharacterDataHandler = parts.append
p.Parse(b"<a>one &amp; two &lt;three&gt;</a>", True)
print(parts)

# namespaces
ns = []
p = expat.ParserCreate(namespace_separator=" ")
p.StartElementHandler = lambda n, a: ns.append(("start", n, sorted(a.items())))
p.EndElementHandler = lambda n: ns.append(("end", n))
p.StartNamespaceDeclHandler = lambda pre, uri: ns.append(("ns", pre, uri))
p.EndNamespaceDeclHandler = lambda pre: ns.append(("endns", pre))
p.Parse(b"<r xmlns='u1' xmlns:p='u2' p:x='1'><p:c/></r>", True)
for e in ns:
    print(e)

# chunked feeding
seen = []
p = expat.ParserCreate()
p.StartElementHandler = lambda n, a: seen.append(n)
for ch in (b"<do", b"c><it", b"em/></d", b"oc>"):
    p.Parse(ch, False)
p.Parse(b"", True)
print(seen)

# errors
for bad in (b"<a><b></a>", b"", b"<a/>junk", b"<a x='1' x='2'/>", b"<a>&nope;</a>", b"<a", b"<1/>"):
    p = expat.ParserCreate()
    try:
        p.Parse(bad, True)
    except expat.ExpatError as e:
        print(bad, e.code, e.lineno, e.offset, e)
        print(expat.ErrorString(e.code), p.ErrorCode, p.ErrorLineNumber, p.ErrorColumnNumber, p.ErrorByteIndex)

print(expat.errors.messages[expat.errors.codes["unclosed token"]])
print(expat.errors.XML_ERROR_TAG_MISMATCH)
print(expat.model.XML_CTYPE_CHOICE, expat.model.XML_CQUANT_REP)

# a handler exception propagates
p = expat.ParserCreate()


def boom(name, attrs):
    raise KeyError(name)


p.StartElementHandler = boom
try:
    p.Parse(b"<a/>", True)
except KeyError as e:
    print("KeyError", e)

# encodings
p = expat.ParserCreate()
txt = []
p.CharacterDataHandler = txt.append
p.Parse('<?xml version="1.0" encoding="iso-8859-1"?><a>caf\xe9</a>'.encode("latin-1"), True)
print(txt)
p = expat.ParserCreate()
txt = []
p.CharacterDataHandler = txt.append
p.Parse("<a>€</a>".encode("utf-16"), True)
print(txt)
p = expat.ParserCreate("utf-8")
txt = []
p.CharacterDataHandler = txt.append
p.Parse(b"<a>\xc3\xa9</a>", True)
print(txt)

# external entities
refs = []
p = expat.ParserCreate()
p.SetParamEntityParsing(expat.XML_PARAM_ENTITY_PARSING_ALWAYS)
p.ExternalEntityRefHandler = lambda ctx, base, sysid, pubid: refs.append((ctx, base, sysid, pubid)) or 1
p.SetBase("http://example/")
print(p.GetBase())
p.Parse(b"<!DOCTYPE a SYSTEM 'a.dtd' [<!ENTITY x SYSTEM 'x.xml'>]><a>&x;</a>", True)
print(refs)

# default handler
dflt = []
p = expat.ParserCreate()
p.DefaultHandler = dflt.append
p.Parse(b"<a><!--x--></a>", True)
print(dflt)
