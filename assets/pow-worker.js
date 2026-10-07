// Satchel: solves the sign-up proof of work off the main thread.
// The page sends { sha256, challenge, difficulty }: the hashed URL of
// sha256.js and what POST /auth/pow returned. The answer is { nonce }.
"use strict";

self.onmessage = (event) => {
  const { sha256, challenge, difficulty } = event.data;
  if (!self.satchelSha256) importScripts(sha256);
  const base64 = challenge.replace(/-/g, "+").replace(/_/g, "/");
  const bytes = Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
  const nonce = self.satchelSha256.solve(bytes, difficulty);
  self.postMessage({ nonce: String(nonce) });
};
