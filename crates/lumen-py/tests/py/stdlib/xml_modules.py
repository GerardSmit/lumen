import io
import xml.etree.ElementTree as ET
import xml.dom.minidom as minidom
import xml.sax
import xml.sax.handler
from xml.dom import pulldom

SRC = "<library><book id='1' lang='en'><title>Dune</title><year>1965</year></book><book id='2'><title>Emma</title></book><!--end--></library>"

root = ET.fromstring(SRC)
print(root.tag, [c.attrib for c in root])
print([t.text for t in root.iter("title")])
print(root.find("book[@id='2']/title").text)
print(root.findall(".//year")[0].text)

new = ET.SubElement(root, "book", id="3")
ET.SubElement(new, "title").text = "Ulysses & more"
print(ET.tostring(root, encoding="unicode"))

tree = ET.ElementTree(root)
buf = io.BytesIO()
tree.write(buf, encoding="utf-8", xml_declaration=True)
print(buf.getvalue()[:60])

ET.indent(root)
print(ET.tostring(root, encoding="unicode").splitlines()[1])

ns = ET.fromstring("<a:r xmlns:a='urn:x'><a:c/></a:r>")
print(ns.tag, ns[0].tag)
print(ET.tostring(ns, encoding="unicode"))

events = [(e, el.tag) for e, el in ET.iterparse(io.StringIO(SRC), events=("start", "end"))]
print(events[:4], len(events))

parser = ET.XMLParser(target=ET.TreeBuilder())
for chunk in ("<r><a>", "x</a>", "</r>"):
    parser.feed(chunk)
print(ET.tostring(parser.close(), encoding="unicode"))

try:
    ET.fromstring("<a><b></a>")
except ET.ParseError as e:
    print("ParseError", e.code, e.position, e)

doc = minidom.parseString(SRC)
print([n.getAttribute("id") for n in doc.getElementsByTagName("book")])
print(doc.documentElement.firstChild.firstChild.firstChild.data)
print(doc.documentElement.toxml()[:50])
print(minidom.parseString("<a><b>1</b><b>2</b></a>").toprettyxml(indent=" "))

d = minidom.Document()
r = d.createElement("root")
d.appendChild(r)
r.setAttribute("k", "v<>")
r.appendChild(d.createTextNode("a & b"))
print(d.toxml())


class H(xml.sax.handler.ContentHandler):
    def __init__(self):
        self.out = []

    def startElement(self, name, attrs):
        self.out.append(("start", name, sorted(attrs.items())))

    def endElement(self, name):
        self.out.append(("end", name))

    def characters(self, content):
        self.out.append(("chars", content))


h = H()
xml.sax.parseString(SRC.encode(), h)
print(h.out[:5], len(h.out))

stream = pulldom.parseString("<a><b/>t</a>")
print([(ev, getattr(node, "tagName", None)) for ev, node in stream])
