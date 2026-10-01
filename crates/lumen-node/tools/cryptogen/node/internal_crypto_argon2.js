'use strict';

// lumen: argon2() and argon2Sync() (the Argon2 KDF of RFC 9106), written to match Node 26's
// lib/internal/crypto/argon2.js without its WebCrypto deriveBits entry point.

const {
  FunctionPrototypeCall,
} = primordials;

const {
  Argon2Job,
  kCryptoJobAsync,
  kCryptoJobSync,
  kTypeArgon2d,
  kTypeArgon2i,
  kTypeArgon2id,
} = internalBinding('crypto');

const {
  validateFunction,
  validateInteger,
  validateObject,
  validateOneOf,
  validateString,
} = require('internal/validators');

const {
  getArrayBufferOrView,
} = require('internal/crypto/util');

const { Buffer } = require('buffer');

const types = {
  __proto__: null,
  argon2d: kTypeArgon2d,
  argon2i: kTypeArgon2i,
  argon2id: kTypeArgon2id,
};

function check(algorithm, parameters) {
  validateString(algorithm, 'algorithm');
  validateOneOf(algorithm, 'algorithm', ['argon2d', 'argon2i', 'argon2id']);
  validateObject(parameters, 'parameters');

  const message = getArrayBufferOrView(parameters.message, 'parameters.message');
  const nonce = getArrayBufferOrView(parameters.nonce, 'parameters.nonce');
  validateInteger(nonce.byteLength, 'parameters.nonce.byteLength', 8, 2 ** 32 - 1);
  const { parallelism, tagLength, memory, passes } = parameters;
  validateInteger(parallelism, 'parameters.parallelism', 1, 2 ** 24 - 1);
  validateInteger(tagLength, 'parameters.tagLength', 4, 2 ** 32 - 1);
  validateInteger(memory, 'parameters.memory', 8 * parallelism, 2 ** 32 - 1);
  validateInteger(passes, 'parameters.passes', 1, 2 ** 32 - 1);

  let { secret, associatedData } = parameters;
  if (secret !== undefined) secret = getArrayBufferOrView(secret, 'parameters.secret');
  if (associatedData !== undefined) {
    associatedData = getArrayBufferOrView(associatedData, 'parameters.associatedData');
  }

  return {
    message, nonce, parallelism, tagLength, memory, passes, secret, associatedData,
    type: types[algorithm],
  };
}

function argon2(algorithm, parameters, callback) {
  parameters = check(algorithm, parameters);

  validateFunction(callback, 'callback');

  const job = new Argon2Job(
    kCryptoJobAsync,
    parameters.message,
    parameters.nonce,
    parameters.parallelism,
    parameters.tagLength,
    parameters.memory,
    parameters.passes,
    parameters.secret,
    parameters.associatedData,
    parameters.type);

  job.ondone = (error, result) => {
    if (error !== undefined)
      return FunctionPrototypeCall(callback, job, error);
    const buf = Buffer.from(result);
    return FunctionPrototypeCall(callback, job, null, buf);
  };

  job.run();
}

function argon2Sync(algorithm, parameters) {
  parameters = check(algorithm, parameters);

  const job = new Argon2Job(
    kCryptoJobSync,
    parameters.message,
    parameters.nonce,
    parameters.parallelism,
    parameters.tagLength,
    parameters.memory,
    parameters.passes,
    parameters.secret,
    parameters.associatedData,
    parameters.type);

  const { 0: err, 1: result } = job.run();

  if (err !== undefined)
    throw err;

  return Buffer.from(result);
}

module.exports = {
  argon2,
  argon2Sync,
};
