// Satchel: copy buttons and Nostr (NIP-07) login. Everything else is
// server-rendered; htmx handles forms and status polling.
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
  }
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
  button.disabled = true;
  try {
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
  } catch (error) {
    say(error && error.message ? error.message : "Nostr login failed.");
  } finally {
    button.disabled = false;
  }
}
