// ---- X.509 certificates as Node's legacy certificate objects ----------------------------------
// The DER of a certificate (from the TLS engine) becomes the object tlsSocket.getPeerCertificate()
// returns: subject / issuer, subjectaltname, infoAccess, key details, validity, fingerprints.

function derNode(buf, pos) {
  const tag = buf[pos];
  let len = buf[pos + 1];
  let headerLength = 2;
  if (len & 0x80) {
    const count = len & 0x7f;
    len = 0;
    for (let i = 0; i < count; i++) len = len * 256 + buf[pos + 2 + i];
    headerLength = 2 + count;
  }
  const start = pos + headerLength;
  if (start + len > buf.length) throw new Error("truncated DER");
  return { tag, start: pos, end: start + len, raw: buf.subarray(pos, start + len), content: buf.subarray(start, start + len) };
}

function derChildren(node) {
  const out = [];
  const content = node.content;
  let pos = 0;
  while (pos < content.length) {
    const child = derNode(content, pos);
    out.push(child);
    pos = child.end;
  }
  return out;
}

function derOid(content) {
  const parts = [];
  let value = 0;
  for (let i = 0; i < content.length; i++) {
    value = value * 128 + (content[i] & 0x7f);
    if (!(content[i] & 0x80)) {
      if (parts.length === 0) {
        const first = value < 80 ? Math.floor(value / 40) : 2;
        parts.push(first, value - first * 40);
      } else {
        parts.push(value);
      }
      value = 0;
    }
  }
  return parts.join(".");
}

const x509NameOids = {
  "2.5.4.3": "CN", "2.5.4.4": "SN", "2.5.4.5": "serialNumber", "2.5.4.6": "C", "2.5.4.7": "L",
  "2.5.4.8": "ST", "2.5.4.9": "street", "2.5.4.10": "O", "2.5.4.11": "OU", "2.5.4.12": "title",
  "2.5.4.15": "businessCategory", "2.5.4.17": "postalCode", "2.5.4.42": "GN", "2.5.4.46": "dnQualifier",
  "1.2.840.113549.1.9.1": "emailAddress", "0.9.2342.19200300.100.1.25": "DC",
  "0.9.2342.19200300.100.1.1": "UID", "1.3.6.1.4.1.311.60.2.1.3": "jurisdictionC",
  "1.3.6.1.4.1.311.60.2.1.2": "jurisdictionST", "1.3.6.1.4.1.311.60.2.1.1": "jurisdictionL",
};

function derString(node) {
  const bytes = node.content;
  switch (node.tag) {
    case 0x1e: {
      let out = "";
      for (let i = 0; i + 1 < bytes.length; i += 2) out += String.fromCharCode((bytes[i] << 8) | bytes[i + 1]);
      return out;
    }
    case 0x14: case 0x1b:
      return Buffer.from(bytes).toString("latin1");
    case 0x1c: {
      let out = "";
      for (let i = 0; i + 3 < bytes.length; i += 4) {
        out += String.fromCodePoint(((bytes[i] << 24) | (bytes[i + 1] << 16) | (bytes[i + 2] << 8) | bytes[i + 3]) >>> 0);
      }
      return out;
    }
    default:
      return Buffer.from(bytes).toString("utf8");
  }
}

function x509Name(node) {
  const out = { __proto__: null };
  for (const set of derChildren(node)) {
    for (const pair of derChildren(set)) {
      const parts = derChildren(pair);
      if (parts.length < 2 || parts[0].tag !== 0x06) continue;
      const oid = derOid(parts[0].content);
      const key = x509NameOids[oid] || oid;
      const value = derString(parts[1]);
      if (key in out) {
        if (Array.isArray(out[key])) out[key].push(value);
        else out[key] = [out[key], value];
      } else {
        out[key] = value;
      }
    }
  }
  return out;
}

const x509Months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
function x509Time(node) {
  const text = derString(node);
  let year;
  let offset;
  if (node.tag === 0x17) {
    const short = Number(text.slice(0, 2));
    year = short >= 50 ? 1900 + short : 2000 + short;
    offset = 2;
  } else {
    year = Number(text.slice(0, 4));
    offset = 4;
  }
  const month = Number(text.slice(offset, offset + 2)) - 1;
  const day = Number(text.slice(offset + 2, offset + 4));
  const time = `${text.slice(offset + 4, offset + 6)}:${text.slice(offset + 6, offset + 8)}:${text.slice(offset + 8, offset + 10) || "00"}`;
  return `${x509Months[month]} ${String(day).padStart(2, " ")} ${time} ${year} GMT`;
}

function hexUpper(bytes) {
  let out = "";
  for (let i = 0; i < bytes.length; i++) out += (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16);
  return out.toUpperCase();
}

function stripLeadingZeros(bytes) {
  let i = 0;
  while (i < bytes.length - 1 && bytes[i] === 0) i++;
  return bytes.subarray(i);
}

function bitLength(bytes) {
  const trimmed = stripLeadingZeros(bytes);
  if (trimmed.length === 0 || trimmed[0] === 0) return 0;
  return (trimmed.length - 1) * 8 + (32 - Math.clz32(trimmed[0]));
}

function x509Ip(bytes) {
  if (bytes.length === 4) return Array.from(bytes).join(".");
  if (bytes.length === 16) {
    const groups = [];
    for (let i = 0; i < 16; i += 2) groups.push(((bytes[i] << 8) | bytes[i + 1]).toString(16).toUpperCase());
    return groups.join(":");
  }
  return hexUpper(bytes);
}

function isSafeAltName(text, utf8) {
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c === 0x22 || c === 0x5c || c === 0x2c || c === 0x27) return false;
    if (utf8) {
      if (c < 0x20 || c === 0x7f) return false;
    } else if (c < 0x20 || c > 0x7e) {
      return false;
    }
  }
  return true;
}

function altNameText(prefix, text, utf8) {
  return isSafeAltName(text, utf8) ? `${prefix}:${text}` : `${prefix}:${JSON.stringify(text)}`;
}

function x509GeneralName(node) {
  switch (node.tag) {
    case 0x81: return altNameText("email", Buffer.from(node.content).toString("latin1"), false);
    case 0x82: return altNameText("DNS", Buffer.from(node.content).toString("latin1"), false);
    case 0x86: return altNameText("URI", Buffer.from(node.content).toString("latin1"), false);
    case 0x87: return `IP Address:${x509Ip(node.content)}`;
    case 0x88: return `Registered ID:${derOid(node.content)}`;
    case 0xa4: {
      const name = x509Name(derChildren(node)[0]);
      const parts = [];
      for (const key of Object.keys(name)) {
        for (const value of [].concat(name[key])) parts.push(`${key}=${value}`);
      }
      return `DirName:${JSON.stringify(parts.join("\n"))}`;
    }
    case 0xa0: return "othername:<unsupported>";
    default: return undefined;
  }
}

function x509Extensions(fields) {
  const result = { ca: false, san: undefined, infoAccess: undefined, usage: undefined };
  const wrapper = fields.find((field) => field.tag === 0xa3);
  if (!wrapper) return result;
  for (const extension of derChildren(derChildren(wrapper)[0])) {
    try {
      const parts = derChildren(extension);
      const oid = derOid(parts[0].content);
      const octets = parts[parts.length - 1];
      if (octets.tag !== 0x04) continue;
      const inner = derNode(octets.content, 0);
      if (oid === "2.5.29.19") {
        result.ca = derChildren(inner).some((node) => node.tag === 0x01 && node.content[0] !== 0);
      } else if (oid === "2.5.29.17") {
        const names = [];
        for (const name of derChildren(inner)) {
          const text = x509GeneralName(name);
          if (text !== undefined) names.push(text);
        }
        result.san = names.join(", ");
      } else if (oid === "1.3.6.1.5.5.7.1.1") {
        let text = "";
        for (const access of derChildren(inner)) {
          const [method, location] = derChildren(access);
          const methodOid = derOid(method.content);
          const label = methodOid === "1.3.6.1.5.5.7.48.1" ? "OCSP" : methodOid === "1.3.6.1.5.5.7.48.2" ? "CA Issuers" : methodOid;
          const name = x509GeneralName(location);
          if (name !== undefined) text += `${label} - ${name}\n`;
        }
        result.infoAccess = text;
      } else if (oid === "2.5.29.37") {
        result.usage = derChildren(inner).map((node) => derOid(node.content));
      }
    } catch {
      // Extensions this parser cannot read are left out, as the others still apply.
    }
  }
  return result;
}

const x509Curves = {
  "1.2.840.10045.3.1.7": ["prime256v1", "P-256"],
  "1.3.132.0.34": ["secp384r1", "P-384"],
  "1.3.132.0.35": ["secp521r1", "P-521"],
  "1.3.132.0.10": ["secp256k1", undefined],
  "1.3.132.0.33": ["secp224r1", "P-224"],
};

function fingerprint(der, algorithm) {
  const hash = __builtins.get("crypto").createHash(algorithm).update(der).digest("hex").toUpperCase();
  return hash.match(/../g).join(":");
}

function derEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// Splits a certificate into the pieces the legacy object and the chain walk need.
function x509Parse(der) {
  const bytes = der instanceof Uint8Array ? der : new Uint8Array(der);
  const cert = derNode(bytes, 0);
  const tbs = derChildren(cert)[0];
  const fields = derChildren(tbs);
  let i = fields[0].tag === 0xa0 ? 1 : 0;
  const serial = fields[i++];
  i++;
  const issuer = fields[i++];
  const validity = derChildren(fields[i++]);
  const subject = fields[i++];
  const spki = fields[i++];
  return { bytes, serial, issuer, validity, subject, spki, rest: fields.slice(i) };
}

function x509Object(der, parsed = x509Parse(der)) {
  const info = {};
  info.subject = x509Name(parsed.subject);
  info.issuer = x509Name(parsed.issuer);
  const extensions = x509Extensions(parsed.rest);
  if (extensions.san !== undefined) info.subjectaltname = extensions.san;
  if (extensions.infoAccess !== undefined) info.infoAccess = extensions.infoAccess;
  info.ca = extensions.ca;
  try {
    const [algorithm, keyBits] = derChildren(parsed.spki);
    const algorithmOid = derOid(derChildren(algorithm)[0].content);
    const keyBytes = keyBits.content.subarray(1);
    if (algorithmOid === "1.2.840.113549.1.1.1") {
      const [modulus, exponent] = derChildren(derNode(keyBytes, 0));
      const n = stripLeadingZeros(modulus.content);
      const e = stripLeadingZeros(exponent.content);
      info.modulus = hexUpper(n);
      info.bits = bitLength(n);
      info.exponent = `0x${hexUpper(e).replace(/^0+(?=.)/, "").toLowerCase()}`;
      info.pubkey = Buffer.from(parsed.spki.raw);
    } else if (algorithmOid === "1.2.840.10045.2.1") {
      const curveOid = derOid(derChildren(algorithm)[1].content);
      const [asn1Curve, nistCurve] = x509Curves[curveOid] || [curveOid, undefined];
      info.bits = (keyBytes.length - 1) * 4;
      info.pubkey = Buffer.from(keyBytes);
      info.asn1Curve = asn1Curve;
      if (nistCurve !== undefined) info.nistCurve = nistCurve;
    }
  } catch {
    // An unreadable public key leaves the key fields out.
  }
  info.valid_from = x509Time(parsed.validity[0]);
  info.valid_to = x509Time(parsed.validity[1]);
  info.fingerprint = fingerprint(der, "sha1");
  info.fingerprint256 = fingerprint(der, "sha256");
  info.fingerprint512 = fingerprint(der, "sha512");
  if (extensions.usage !== undefined) info.ext_key_usage = extensions.usage;
  info.serialNumber = hexUpper(stripLeadingZeros(parsed.serial.content));
  info.raw = Buffer.from(der);
  return info;
}

// The peer's chain as nested certificate objects, each linked to the certificate that issued it
// (a self-signed one links to itself).
function x509Chain(chain, detailed) {
  if (chain.length === 0) return undefined;
  const parsed = chain.map((der) => x509Parse(der));
  const objects = parsed.map((entry, index) => x509Object(chain[index], entry));
  if (!detailed) return objects[0];
  for (let i = 0; i < objects.length; i++) {
    const issuerDer = parsed[i].issuer.raw;
    if (derEqual(issuerDer, parsed[i].subject.raw)) {
      objects[i].issuerCertificate = objects[i];
      continue;
    }
    for (let j = 0; j < objects.length; j++) {
      if (j === i) continue;
      if (derEqual(issuerDer, parsed[j].subject.raw)) {
        objects[i].issuerCertificate = objects[j];
        break;
      }
    }
  }
  return objects[0];
}
