// SHA-256 (FIPS 180-4) in plain JavaScript, for the sign-up proof of work.
// Works in pages and workers: it defines self.satchelSha256 = { digest, solve }.
//
// Test vector shared with src/pow.rs: the 41 challenge bytes 0x00..0x28
// (base64url "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJyg")
// at difficulty 16 are first solved by nonce 1457:
// SHA-256(challenge || 1457 as u64 big-endian)
//   = 000010261cce78dd115e49ee09495c4c6d009ee79b4168c3f5ff1f6cee166e91
// (19 leading zero bits). At difficulty 12 the first nonce is 1063.
"use strict";

(() => {
  const K = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ]);
  const H0 = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  const W = new Uint32Array(64);

  // One 64-byte block at `offset` of `view` (a DataView) into `state`.
  function compress(state, view, offset) {
    for (let i = 0; i < 16; i++) W[i] = view.getUint32(offset + i * 4);
    for (let i = 16; i < 64; i++) {
      const a = W[i - 15];
      const b = W[i - 2];
      const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
      const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
      W[i] = (W[i - 16] + s0 + W[i - 7] + s1) | 0;
    }
    let a = state[0];
    let b = state[1];
    let c = state[2];
    let d = state[3];
    let e = state[4];
    let f = state[5];
    let g = state[6];
    let h = state[7];
    for (let i = 0; i < 64; i++) {
      const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
      const t1 = (h + S1 + ((e & f) ^ (~e & g)) + K[i] + W[i]) | 0;
      const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
      const t2 = (S0 + ((a & b) ^ (a & c) ^ (b & c))) | 0;
      h = g;
      g = f;
      f = e;
      e = (d + t1) | 0;
      d = c;
      c = b;
      b = a;
      a = (t1 + t2) | 0;
    }
    state[0] += a;
    state[1] += b;
    state[2] += c;
    state[3] += d;
    state[4] += e;
    state[5] += f;
    state[6] += g;
    state[7] += h;
  }

  // Message, 0x80, zeros, and the length in bits, in whole 64-byte blocks.
  function pad(bytes) {
    const blocks = Math.ceil((bytes.length + 9) / 64);
    const padded = new Uint8Array(blocks * 64);
    padded.set(bytes);
    padded[bytes.length] = 0x80;
    const view = new DataView(padded.buffer);
    const bits = bytes.length * 8;
    view.setUint32(padded.length - 8, Math.floor(bits / 0x100000000));
    view.setUint32(padded.length - 4, bits >>> 0);
    return view;
  }

  function digest(bytes) {
    const view = pad(bytes);
    const state = new Uint32Array(H0);
    for (let offset = 0; offset < view.byteLength; offset += 64) compress(state, view, offset);
    const out = new Uint8Array(32);
    const outView = new DataView(out.buffer);
    for (let i = 0; i < 8; i++) outView.setUint32(i * 4, state[i]);
    return out;
  }

  function leadingZeroBits(state) {
    let bits = 0;
    for (let i = 0; i < 8; i++) {
      const zeros = Math.clz32(state[i]);
      bits += zeros;
      if (zeros < 32) break;
    }
    return bits;
  }

  // The smallest nonce whose SHA-256(challenge || nonce as u64 big-endian)
  // starts with `difficulty` zero bits. A 41-byte challenge plus the nonce
  // and padding fit in one block, so each try is one compression.
  function solve(challenge, difficulty) {
    if (challenge.length + 8 > 55) throw new Error("challenge too long");
    const message = new Uint8Array(challenge.length + 8);
    message.set(challenge);
    const view = pad(message);
    const at = challenge.length;
    const state = new Uint32Array(8);
    for (let nonce = 0; nonce <= Number.MAX_SAFE_INTEGER; nonce++) {
      view.setUint32(at, Math.floor(nonce / 0x100000000));
      view.setUint32(at + 4, nonce >>> 0);
      state.set(H0);
      compress(state, view, 0);
      if (leadingZeroBits(state) >= difficulty) return nonce;
    }
    throw new Error("no solution");
  }

  self.satchelSha256 = { digest, solve };
})();
