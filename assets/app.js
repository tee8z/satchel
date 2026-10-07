// Satchel: copy buttons, Nostr (NIP-07) login, the QR scanner on Send, and
// the sign-up proof of work. Everything else is server-rendered; htmx handles
// forms and status polling.
"use strict";

document.addEventListener("click", (event) => {
  const target = event.target instanceof Element ? event.target : null;
  const copy = target && target.closest("[data-copy]");
  if (copy) {
    copyText(copy);
    return;
  }
  const nostr = target && target.closest("[data-nostr]");
  if (nostr) {
    event.preventDefault();
    nostrAuth(nostr);
    return;
  }
  const scan = target && target.closest("[data-scan]");
  if (scan) {
    startScan(scan);
    return;
  }
  if (target && target.closest("[data-scan-stop]")) stopScan();
});

async function copyText(button) {
  const label = button.textContent;
  try {
    await navigator.clipboard.writeText(button.dataset.copy);
    button.textContent = "Copied";
  } catch {
    button.textContent = "Copy failed";
  }
  setTimeout(() => {
    button.textContent = label;
  }, 1500);
}

async function postJson(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    credentials: "same-origin",
  });
  try {
    return await response.json();
  } catch {
    return { error: `Request failed (${response.status}).` };
  }
}

// Proof of work for new wallets (see src/pow.rs). A form that creates an
// account carries hidden pow_challenge/pow_nonce fields and a marker with the
// worker's URL; solving starts as soon as the page loads, so it is usually
// done before the form is filled in.
const powMarker = document.querySelector("[data-pow-worker]");
const pow = powMarker ? powSolver(powMarker) : null;

function powSolver(marker) {
  const form = marker.closest("form");
  const field = (name) => form && form.querySelector(`input[name="${name}"]`);
  const fields = { challenge: field("pow_challenge"), nonce: field("pow_nonce") };
  let worker = null;
  let solution = null;
  let current = null;

  function start() {
    if (worker) worker.terminate();
    solution = null;
    if (fields.challenge) fields.challenge.value = "";
    if (fields.nonce) fields.nonce.value = "";
    current = (async () => {
      const issued = await postJson("/auth/pow", {});
      if (!issued.challenge) throw new Error(issued.error || "Could not prepare the sign-up check.");
      const nonce = await new Promise((resolve, reject) => {
        worker = new Worker(marker.dataset.powWorker);
        worker.onmessage = (event) => resolve(event.data.nonce);
        worker.onerror = () => reject(new Error("The sign-up check failed in this browser."));
        worker.postMessage({
          sha256: marker.dataset.powSha256,
          challenge: issued.challenge,
          difficulty: issued.difficulty,
        });
      });
      worker.terminate();
      worker = null;
      if (fields.challenge) fields.challenge.value = issued.challenge;
      if (fields.nonce) fields.nonce.value = nonce;
      solution = { challenge: issued.challenge, nonce, expiresAt: issued.expires_at };
      return solution;
    })();
    // Failures surface when the form is submitted.
    current.catch(() => {});
    return current;
  }

  // Solved, with at least half a minute left to use it.
  function fresh() {
    return solution !== null && solution.expiresAt * 1000 - Date.now() > 30000;
  }

  // A usable solution: the current one, or a new one if it failed or is about to expire.
  async function ready() {
    try {
      await current;
    } catch {
      return start();
    }
    return fresh() ? solution : start();
  }

  if (form) {
    const button = form.querySelector('button[type="submit"]');
    const status = form.querySelector(".pow-status");
    const label = button ? button.textContent : "";
    const reset = () => {
      if (button) {
        button.disabled = false;
        button.textContent = label;
      }
    };
    form.addEventListener("submit", async (event) => {
      if (fresh()) return;
      event.preventDefault();
      if (button) {
        button.disabled = true;
        button.textContent = "Preparing…";
      }
      try {
        await ready();
        form.submit();
      } catch (error) {
        if (status) status.textContent = error && error.message ? error.message : "The sign-up check failed.";
        reset();
      }
    });
    // Back from another page: the solution may be spent already.
    window.addEventListener("pageshow", (event) => {
      if (!event.persisted) return;
      reset();
      start();
    });
  }

  start();
  return { ready, restart: start };
}

async function nostrAuth(button) {
  const status = button.parentElement.querySelector(".nostr-status");
  const say = (text) => {
    if (status) status.textContent = text;
  };
  if (!window.nostr) {
    say("No Nostr signer found. Install a NIP-07 extension such as Alby or nos2x.");
    return;
  }
  const mode = button.dataset.nostr;
  const body = { mode };
  if (mode === "signup") {
    const input = document.getElementById("username");
    body.username = input ? input.value.trim() : "";
    if (!body.username) {
      say("Choose a username above first.");
      return;
    }
  }
  if (mode === "link") body.csrf = button.dataset.csrf;
  if (button.dataset.next) body.next = button.dataset.next;
  button.disabled = true;
  try {
    if (mode === "signup" && pow) {
      say("Preparing…");
      const solution = await pow.ready();
      body.pow_challenge = solution.challenge;
      body.pow_nonce = solution.nonce;
      say("");
    }
    const challenge = await postJson("/auth/nostr/challenge", {});
    if (challenge.error) throw new Error(challenge.error);
    body.event = await window.nostr.signEvent({
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ["u", challenge.url],
        ["method", "POST"],
        ["challenge", challenge.challenge],
      ],
      content: "",
    });
    const result = await postJson("/auth/nostr", body);
    if (result.redirect) {
      window.location.assign(result.redirect);
      return;
    }
    say(result.error || "Nostr login failed.");
    // The attempt may have used up the solution; prepare the next one.
    if (mode === "signup" && pow) pow.restart();
  } catch (error) {
    say(error && error.message ? error.message : "Nostr login failed.");
  } finally {
    button.disabled = false;
  }
}

// QR scanning fills the Send field from the camera. It needs the browser's
// BarcodeDetector (Chromium on Android, ChromeOS, and macOS); elsewhere the
// Scan button stays hidden and people paste instead.
const canScan =
  "BarcodeDetector" in window && !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
let scanning = null;

function showScanners() {
  if (scanning && !scanning.view.isConnected) stopScan();
  for (const box of document.querySelectorAll("[data-scanner][hidden]")) box.hidden = false;
}

if (canScan) {
  showScanners();
  // htmx replaces the Send section after each payment; show the new scanner too.
  new MutationObserver(showScanners).observe(document.body, { childList: true, subtree: true });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopScan();
  });
}

// What a QR code holds, as the Send field takes it: BIP21 URIs give their
// lightning= invoice, a lightning: prefix is dropped, and anything that is not
// an invoice, LNURL, or Lightning Address is ignored.
function lightningText(raw) {
  const text = String(raw || "").trim();
  if (/^bitcoin:/i.test(text)) {
    const query = text.split("?")[1] || "";
    for (const [key, value] of new URLSearchParams(query)) {
      if (key.toLowerCase() === "lightning" && value.trim()) return value.trim();
    }
    return null;
  }
  const value = text.replace(/^lightning:/i, "").trim();
  return /^ln/i.test(value) || /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(value) ? value : null;
}

async function startScan(button) {
  const box = button.closest("[data-scanner]");
  const view = box.querySelector("[data-scan-view]");
  const video = view.querySelector("video");
  const status = box.querySelector(".scan-status");
  const field = document.getElementById(button.dataset.scan);
  const say = (text) => {
    status.textContent = text;
    status.hidden = !text;
  };
  stopScan();
  say("");
  button.disabled = true;
  try {
    const formats = await window.BarcodeDetector.getSupportedFormats();
    if (!formats.includes("qr_code")) throw new Error("This browser cannot read QR codes.");
    const detector = new window.BarcodeDetector({ formats: ["qr_code"] });
    const stream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },
      audio: false,
    });
    scanning = { stream, view, button };
    view.hidden = false;
    video.srcObject = stream;
    await video.play();
    say("Point the camera at a Lightning QR code.");
    const look = async () => {
      if (!scanning || scanning.stream !== stream) return;
      try {
        const codes = await detector.detect(video);
        if (codes.length) {
          const value = lightningText(codes[0].rawValue);
          if (value) {
            field.value = value;
            stopScan();
            say("Scanned. Check the details, then send.");
            field.focus();
            return;
          }
          say("That QR code is not a Lightning invoice or address.");
        }
      } catch {
        // The first frames may not be ready yet; keep looking.
      }
      setTimeout(look, 250);
    };
    look();
  } catch (error) {
    stopScan();
    button.disabled = false;
    say(
      error && error.name === "NotAllowedError"
        ? "Camera access was refused. Allow it in the browser settings, or paste instead."
        : (error && error.message) || "Could not start the camera.",
    );
  }
}

function stopScan() {
  if (!scanning) return;
  const { stream, view, button } = scanning;
  scanning = null;
  for (const track of stream.getTracks()) track.stop();
  const video = view.querySelector("video");
  if (video) video.srcObject = null;
  view.hidden = true;
  button.disabled = false;
}
