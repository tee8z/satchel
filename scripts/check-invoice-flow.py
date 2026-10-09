#!/usr/bin/env python3
"""Browser regressions against `cargo test browser_fixture -- --ignored --nocapture`.

Requires Python Playwright and Chromium. Pass --chromium for a system browser.
All invoices and balances come from the local fake-node fixture.
"""
import argparse
import json
from urllib.request import urlopen

from playwright.sync_api import expect, sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--chromium")
args = parser.parse_args()
base = "http://127.0.0.1:18097"


def fixture():
    with urlopen(base + "/fixture/invoices") as response:
        return json.load(response)


def login(page):
    page.locator('[name="username"]').fill("preview")
    page.locator('[name="password"]').fill("preview-only-password")
    page.get_by_role("button", name="Log in", exact=True).click()


def handoff(page, data):
    # localhost and 127.0.0.1 are separate sites. This is a cross-site POST,
    # so Satchel's SameSite=Lax cookie returns only on the redirect GET.
    page.goto("http://localhost:18097/fixture/invoices")
    page.evaluate("""({base, data}) => {
        const form = document.createElement('form');
        form.method = 'POST';
        form.action = base + '/auth/nostr/handoff';
        for (const [name, value] of Object.entries({
            event: JSON.stringify(data.handoff), next: '/launch/lightning/' + data.fixed
        })) {
            const input = document.createElement('input');
            input.type = 'hidden'; input.name = name; input.value = value;
            form.append(input);
        }
        document.body.append(form); form.submit();
    }""", {"base": base, "data": data})


with sync_playwright() as playwright:
    browser = playwright.chromium.launch(
        executable_path=args.chromium, headless=True, args=["--disable-dev-shm-usage"]
    )
    context = browser.new_context(viewport={"width": 390, "height": 844})
    page = context.new_page()
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    data = fixture()
    handoff(page, data)
    page.wait_for_url(base + "/login?**")
    expect(page.locator('[name="next"]')).to_have_value("/launch/lightning/" + data["fixed"])
    login(page)
    page.wait_for_url(base + "/launch/lightning/" + data["fixed"])
    expect(page.locator("#send-amount")).to_have_value("2100")
    expect(page.locator("#send-amount")).not_to_be_editable()
    expect(page.get_by_role("button", name="Review payment")).to_be_visible()

    # A second tab from the game uses the existing wallet despite its unknown key.
    other = context.new_page()
    handoff(other, fixture())
    other.wait_for_url(base + "/launch/lightning/" + data["fixed"])
    expect(other.locator("#send-amount")).to_have_value("2100")
    expect(other.locator("#send-amount")).not_to_be_editable()
    other.close()

    page.get_by_role("button", name="Review payment").click()
    expect(page.get_by_role("button", name="Confirm and send")).to_be_visible()
    page.get_by_role("button", name="Edit payment").click()
    expect(page.locator("#send-amount")).to_have_value("2100")
    expect(page.locator("#send-amount")).not_to_be_editable()

    # Replacing a fixed invoice clears its amount and enables whole-sat input.
    page.locator("#send-to").fill(data["amountless"])
    expect(page.locator("#send-amount")).to_be_editable()
    expect(page.locator("#send-amount")).to_have_value("")
    page.locator("#send-amount").fill("1.5")
    page.get_by_role("button", name="Review payment").click()
    expect(page.get_by_role("alert")).to_have_text("Enter a whole number of sats.")
    page.locator("#send-amount").fill("21")
    page.get_by_role("button", name="Review payment").click()
    expect(page.locator(".review-amount")).to_have_text("21 sats")
    page.get_by_role("button", name="Edit payment").click()
    expect(page.locator("#send-amount")).to_have_value("21")
    expect(page.locator("#send-amount")).to_be_editable()

    # Pasting and the scanner's input event use the same amount decoder.
    page.locator("#send-to").fill(data["fixed"])
    expect(page.locator("#send-amount")).to_have_value("2100")
    expect(page.locator("#send-amount")).not_to_be_editable()
    page.evaluate("""invoice => {
        const field = document.getElementById('send-to');
        field.value = invoice; field.dispatchEvent(new Event('input', {bubbles: true}));
    }""", data["fractional"])
    expect(page.locator("#send-amount")).to_have_value("21.123")
    expect(page.locator("#send-amount")).not_to_be_editable()
    page.get_by_role("button", name="Review payment").click()
    expect(page.locator(".review-amount")).to_have_text("21.123 sats")
    page.get_by_role("button", name="Edit payment").click()
    page.locator("#send-to").fill("friend@127.0.0.1:18097")
    expect(page.locator("#send-amount")).to_be_editable()
    expect(page.locator("#send-amount")).to_have_value("")
    page.locator("#send-to").fill(data["fixed"])
    page.locator("#send-to").fill(data["amountless"])
    expect(page.locator("#send-amount")).to_be_editable()
    expect(page.locator("#send-amount")).to_have_value("")

    # Forms also work with JavaScript disabled, including a fixed msat invoice.
    plain = browser.new_context(java_script_enabled=False)
    plain_page = plain.new_page()
    plain_page.goto(base + "/launch/lightning/" + data["fractional"])
    login(plain_page)
    expect(plain_page.locator("#send-amount")).to_have_value("21.123")
    plain_page.get_by_role("button", name="Review payment").click()
    expect(plain_page.locator(".review-amount")).to_have_text("21.123 sats")
    plain_page.get_by_role("button", name="Edit payment").click()
    expect(plain_page.locator("#send-amount")).not_to_be_editable()
    plain.close()
    assert not errors, errors
    browser.close()
    print("PASS: cross-site handoff, login, existing session, invoice loading, whole-sat entry, editing, and no-JavaScript forms")
