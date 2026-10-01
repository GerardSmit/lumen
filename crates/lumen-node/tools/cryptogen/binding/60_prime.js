// ---- prime generation and testing --------------------------------------------------------------------

function outOfRange(message) {
  return cryptoError(RangeError, "ERR_OUT_OF_RANGE", message);
}

function bitLength(bytes) {
  const t = trimLeadingZeros(bytes);
  return t.byteLength === 1 && t[0] === 0 ? 0 : (t.byteLength - 1) * 8 + (32 - Math.clz32(t[0]));
}

function compareUnsigned(a, b) {
  const x = trimLeadingZeros(a);
  const y = trimLeadingZeros(b);
  if (x.byteLength !== y.byteLength) return x.byteLength - y.byteLength;
  for (let i = 0; i < x.byteLength; i++) {
    if (x[i] !== y[i]) return x[i] - y[i];
  }
  return 0;
}

class RandomPrimeJob extends CryptoJob {
  constructor(mode, size, safe, add, rem) {
    super(mode);
    const addBytes = add === undefined ? undefined : new Uint8Array(bytesOf(add));
    const remBytes = rem === undefined || addBytes === undefined ? undefined : new Uint8Array(bytesOf(rem));
    if (addBytes !== undefined) {
      if (bitLength(addBytes) > size) throw outOfRange("invalid options.add");
      if (remBytes !== undefined && compareUnsigned(addBytes, remBytes) <= 0) throw outOfRange("invalid options.rem");
    }
    this.args = [size, safe, addBytes, remBytes];
  }

  _run() {
    try {
      return toArrayBuffer(__rc.primeGenerate(...this.args));
    } catch (err) {
      throw cipherError(err);
    }
  }

  _runAsync() {
    return __rc.primeGenerateAsync(...this.args).then(toArrayBuffer, (err) => {
      throw cipherError(err);
    });
  }
}

class CheckPrimeJob extends CryptoJob {
  constructor(mode, candidate, checks) {
    super(mode);
    this.args = [new Uint8Array(bytesOf(candidate)), checks];
  }

  _run() {
    return __rc.primeCheck(...this.args);
  }

  _runAsync() {
    return __rc.primeCheckAsync(...this.args);
  }
}

Object.assign(cryptoBinding, { RandomPrimeJob, CheckPrimeJob });
