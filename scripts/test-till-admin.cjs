const { chromium } = require('playwright');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

// Run with Node, Playwright and its Chromium browser (or set CHROME_BIN). No server or phone is needed.
const src = path.join(__dirname, '../host/sidecar/src');
const html = fs.readFileSync(path.join(src, 'admin.html'), 'utf8');
const csp = fs.readFileSync(path.join(src, 'admin.rs'), 'utf8').match(/HeaderValue::from_static\("(default-src [^"]+)"\)/)[1];
const settings = { mode: 'auto', reserve_sat: 10000, threshold_sat: 50000, max_fee_sat: 100, max_fee_ppm: 5000, interval_minutes: 1440 };
const qr = { text: 'disposable-example', qr_svg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><rect width="1" height="1"/></svg>' };

(async () => {
  const browser = await chromium.launch({
    executablePath: process.env.CHROME_BIN,
    headless: true,
    args: ['--no-first-run'],
  });
  try {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
    const page = await context.newPage();
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    const amounts = [], pairingPayloads = [], manualPayments = [], settingsPatches = [], unconfirmedRequests = [];
    let version = 1700000000000;
    let joined = false, destination = null, proposed = null, records = [], interrupted = true;
    let posUrl = 'https://merchant-pos.test', available = true, blocked = null, lightningBlocked = null;
    await context.route('**/*', async (route) => {
      const req = route.request(), u = new URL(req.url()), p = u.pathname;
      assert.equal(u.origin, 'http://merchant.test');
      if (p === '/') return route.fulfill({ contentType: 'text/html', headers: { 'content-security-policy': csp }, body: html });
      if (p === '/assets/figtree.ttf') return route.fulfill({ path: path.join(src, 'assets/figtree.ttf'), contentType: 'font/ttf' });
      const send = (json, status = 200) => route.fulfill({ status, json });
      if (req.headers().authorization !== 'Bearer test-only-review') return send({ error: 'unauthorized' }, 401);
      const d = req.postDataJSON();
      if (p === '/admin/status') return send({
        joined, connected: false, starting: false, pos_url: posUrl, wallet_balance_msat: 100000000,
        federation_id: 'abcdef0123456789'.repeat(4), federation_reachable: true,
      });
      if (p === '/admin/join') { joined = true; return send({ ok: true }); }
      if (p === '/admin/payout') return send({
        settings, destination, pending_destination: proposed, recent: records, blocked,
        funds: available ? { before_withdrawal_fees_sat: 90000, available_sat: 89500, obligations_sat: 0, reserve_sat: 10000, lightning_blocked: lightningBlocked } : null,
      });
      if (p === '/admin/sales') return send({ sales: [{ id: 1, ts_unix_ms: 1795567336000, price_msat: 4000000 }] });
      if (p === '/admin/pair') { pairingPayloads.push(d); return send(qr); }
      if (p === '/admin/claim') { assert.equal(d.name, 'Test counter'); return send(qr); }
      if (p === '/admin/payout/destination') {
        proposed = { address: d.address, callback_host: 'example.test', confirm_token: 'test-confirm' };
        return send({ pending_destination: proposed });
      }
      if (p === '/admin/payout/destination/confirm') {
        assert.equal(d.confirm_token, proposed.confirm_token);
        destination = { address: proposed.address, confirmed_at_ms: ++version };
        proposed = null;
        return send({ ok: true });
      }
      if (p === '/admin/payout/settings') { settingsPatches.push(d); Object.assign(settings, d); return send({ ok: true }); }
      if (p === '/admin/payout/lightning') {
        manualPayments.push(d);
        const original = records.find((r) => r.request_id === d.request_id);
        if (original) return send({ payout: original });
        if (d.expected_destination !== destination?.address || d.expected_confirmed_at_ms !== destination?.confirmed_at_ms)
          return send({ ok: false, error: 'destination_changed', detail: 'The reviewed Lightning destination changed. Refresh and review again.' }, 409);
        const rec = { id: 'record-' + d.request_id, amount_sat: d.amount_sat, request_id: d.request_id, state: 'paid', kind: 'lightning', address: destination.address };
        records.unshift(rec);
        if (d.amount_sat === 2400) return route.abort('connectionreset');
        return send({ payout: rec });
      }
      if (p === '/admin/payout/ecash') {
        if (d.amount_sat === 1700) {
          unconfirmedRequests.push(d.request_id);
          return unconfirmedRequests.length === 1
            ? send({ ok: false, error: 'unknown', detail: 'Check this request again.' }, 409)
            : send({ ok: false, error: 'insufficient_available', detail: 'The till refused this amount.' }, 409);
        }
        if (d.amount_sat === 999999) return send({ ok: false, error: 'insufficient_available', detail: 'Amount exceeds available proceeds' }, 409);
        amounts.push(d);
        const original = records.find((r) => r.request_id === d.request_id);
        if (original) return send({ payout: original });
        if (interrupted) { interrupted = false; return route.abort('connectionreset'); }
        const rec = { id: 'record-' + d.request_id, amount_sat: d.amount_sat, request_id: d.request_id, state: 'exported', kind: 'ecash' };
        records.unshift(rec);
        return send({ payout: rec });
      }
      if (p.startsWith('/admin/payout/records/')) {
        const rec = records.find((r) => p.endsWith('/' + r.id));
        assert.ok(rec);
        return send({ payout: { ...rec, ...(rec.kind === 'ecash' ? { notes: 'disposable-test-note' } : {}) } });
      }
      throw Error('Unexpected fixture route ' + p);
    });

    const byId = (id) => page.locator('#' + id);
    const click = (id) => byId(id).click();
    const tab = (name) => page.getByRole('tab', { name, exact: true });
    const submit = (id) => byId(id).locator('button').first().click();
    const see = (text) => page.getByText(text, { exact: true }).waitFor();
    const refresh = async () => {
      const loaded = page.waitForResponse((r) => r.url().endsWith('/admin/payout'));
      await click('refresh'); await loaded;
      await page.waitForFunction(() => !document.getElementById('refresh').disabled);
    };
    const pairing = async (expected) => {
      await submit('pair'); await page.locator('dialog[open] img').waitFor();
      assert.deepEqual(pairingPayloads.at(-1), expected);
      await click('dismiss');
    };

    await page.goto('http://merchant.test/');
    await page.evaluate(() => document.fonts.ready);
    assert.ok(await page.evaluate(() => [...document.fonts].some((font) => font.family === 'Figtree' && font.status === 'loaded')));
    await page.getByLabel('Till admin password').fill('wrong'); await submit('auth'); await see('Check your till admin password.');
    await page.getByLabel('Till admin password').fill('test-only-review'); await submit('auth');
    await see('This till receives sales in this federation. Use Card 4.0 cards and a 4.0 registry.');
    await page.getByLabel('Federation invite').fill('local-test-invite'); await submit('join'); await click('agree');
    await see('100,000 sats'); await byId('sales').getByText('Sale 1', { exact: false }).waitFor();
    assert.equal(await tab('Overview').getAttribute('aria-selected'), 'true');
    assert.equal(await byId('withdraw').isVisible(), false);
    await tab('Overview').focus(); await page.keyboard.press('ArrowRight');
    assert.ok(await tab('Payouts').evaluate((el) => el === document.activeElement));
    await page.keyboard.press('End'); assert.equal(await tab('Pairing').getAttribute('aria-selected'), 'true');
    await page.keyboard.press('Home'); assert.equal(await tab('Overview').getAttribute('aria-selected'), 'true');
    await page.keyboard.press('ArrowLeft'); assert.equal(await tab('Pairing').getAttribute('aria-selected'), 'true');
    assert.equal(await byId('pairField').isVisible(), false);
    await pairing({});
    await click('changePair'); await page.getByLabel('POS address').fill('https://different-pos.test');
    await pairing({ url: 'https://different-pos.test' });
    await click('changePair'); await pairing({});
    posUrl = null; await refresh();
    assert.ok(await page.getByLabel('POS address').isVisible());
    const previous = pairingPayloads.length;
    await submit('pair'); assert.equal(pairingPayloads.length, previous);
    await page.getByLabel('POS address').fill('https://fallback-pos.test'); await pairing({ url: 'https://fallback-pos.test' });
    await page.getByLabel('Till name').fill('Test counter'); await submit('claim');
    await page.locator('dialog[open] img').waitFor(); await click('dismiss');

    await tab('Payouts').tap();
    await see('Add and confirm a Lightning address to receive payouts.');
    assert.match(await byId('lightningReason').textContent(), /automatic sweeps only/);
    assert.equal(await byId('lnChoice').isDisabled(), true);
    await click('toMode'); assert.ok(await page.getByLabel('Lightning payout mode').evaluate((el) => el === document.activeElement));
    await click('toAddress'); assert.ok(await page.getByLabel('Payout address').evaluate((el) => el === document.activeElement));
    await page.getByLabel('Payout address').fill('counter@example.test'); await submit('address');
    await click('dismiss'); await refresh(); await see('Your Lightning address is waiting for confirmation.');
    await click('toAddress'); await click('agree'); await see('Lightning destination: counter@example.test');
    await click('toMode'); await page.getByLabel('Lightning payout mode').selectOption('off'); await submit('options'); await click('agree');
    await page.getByText('Lightning payouts are off.', { exact: false }).waitFor();
    await click('toMode'); await page.getByLabel('Lightning payout mode').selectOption('both'); await submit('options'); await click('agree');
    await see('Payout settings saved.');
    assert.equal(await page.getByLabel('Withdraw with').inputValue(), 'lightning');
    lightningBlocked = { error: 'fee_unbounded', reason: 'no_lightning_module', detail: 'This test uses structured status.' };
    await refresh(); await see('This federation does not support Lightning payouts. Withdraw eCash to a wallet in the same federation.');
    assert.equal(await byId('toMode').isVisible(), false);
    assert.equal(await byId('lnChoice').isDisabled(), true);
    assert.equal(await byId('reviewWithdrawal').isEnabled(), true);
    lightningBlocked = { error: 'fee_unbounded', detail: 'Lightning payouts are off: this federation has no Lightning module the till can use' };
    await refresh(); await see('This federation does not support Lightning payouts. Withdraw eCash to a wallet in the same federation.');
    lightningBlocked = { error: 'fee_unbounded', detail: 'The fee schedule cannot be bounded.' };
    await refresh(); await see('The till cannot verify Lightning fees. You can withdraw eCash to a wallet in the same federation.');
    lightningBlocked = null; available = false; blocked = { error: 'obligations_unknown', detail: 'Could not check the balance.' };
    await refresh(); await see('The till cannot verify how much is available. Refresh to check again before withdrawing.');
    assert.ok(await byId('reviewWithdrawal').isDisabled());
    available = true; blocked = { error: 'halted', detail: 'This withdrawal needs review.' };
    await refresh(); await see('Payouts are paused. Review the payout history, then resume in payout settings.');
    await click('toMode'); assert.ok(await byId('resume').evaluate((el) => el === document.activeElement));
    assert.ok(await byId('reviewWithdrawal').isDisabled());
    blocked = { error: 'destination_changed', detail: 'The provider changed.' };
    await refresh(); await see('Your Lightning address has changed. Check and confirm it again.');
    assert.ok(await byId('reviewWithdrawal').isEnabled()); assert.ok(await byId('lnChoice').isDisabled());
    blocked = null; await refresh();
    await page.getByLabel('Withdraw with').selectOption('ecash');
    const withdraw = async (amount) => { await page.getByLabel('Amount in sats').fill(amount); await submit('withdraw'); await click('agree'); };
    await withdraw('999999'); await see('Amount exceeds available proceeds');
    assert.ok(await byId('withdraw').isVisible());
    await withdraw('1700'); await see('Check this request again.');
    assert.ok(await byId('withdraw').isHidden());
    await click('recoverWithdrawal'); await click('agree'); await see('The till refused this amount.');
    assert.ok(await byId('withdraw').isVisible());
    assert.equal(unconfirmedRequests.length, 2); assert.equal(unconfirmedRequests[0], unconfirmedRequests[1]);
    await withdraw('1000'); await see('Failed to fetch');
    await click('recoverWithdrawal'); await click('agree');
    await page.locator('dialog[open] textarea').waitFor();
    assert.equal(await byId('code').inputValue(), 'disposable-test-note');
    assert.equal(amounts.length, 2); assert.equal(amounts[0].request_id, amounts[1].request_id);
    await click('dismiss'); assert.equal(await byId('code').inputValue(), '');

    await click('newWithdrawal');
    assert.equal(await page.getByLabel('Amount in sats').inputValue(), '');

    // Both merchant sessions use the destination and revision shown on their review screen.
    const other = await context.newPage();
    await other.goto('http://merchant.test/');
    await other.getByLabel('Till admin password').fill('test-only-review');
    await other.locator('#auth button').click(); await other.getByRole('tab', { name: 'Payouts' }).click();
    // Each merchant reviews and saves the individual settings they edited.
    await other.locator('#payoutSettings > summary').click(); await other.locator('#sweepLimits summary').click();
    await other.getByLabel('Keep in till (sats)').fill('12000'); await other.getByLabel('Fee cap (sats)').fill('150');
    await other.locator('#options button').click();
    assert.equal(await other.getByRole('dialog', { name: 'Save payout settings?' }).getAttribute('aria-describedby'), 'dialogInfo');
    const review = await other.locator('#dialogInfo').textContent();
    assert.match(review, /Keep in till \(sats\) changes from 10,000 to 12,000/);
    assert.match(review, /Fee cap \(sats\) changes from 100 to 150/);
    await other.locator('#agree').click(); await other.getByText('Payout settings saved.', { exact: true }).waitFor();
    assert.deepEqual(settingsPatches.at(-1), { reserve_sat: 12000, max_fee_sat: 150 });
    await page.getByLabel('Lightning payout mode').selectOption('manual'); await submit('options');
    assert.equal(await byId('dialogInfo').textContent(), 'Lightning payout mode changes from Automatic and manual to Manual only.');
    await click('agree'); await see('Payout settings saved.');
    assert.deepEqual(settingsPatches.at(-1), { mode: 'manual' });
    assert.equal(settings.reserve_sat, 12000); assert.equal(settings.max_fee_sat, 150);
    const oldDestination = { ...destination };
    await page.getByLabel('Withdraw with').selectOption('lightning');
    await page.getByLabel('Amount in sats').fill('2100'); await submit('withdraw');
    await page.getByText('2,100 sats to ' + oldDestination.address, { exact: true }).waitFor();
    await other.locator('#addressDetails summary').click();
    await other.getByLabel('Payout address').fill('second@example.test');
    await other.locator('#address button').click(); await other.locator('#agree').click();
    await other.getByText('Lightning destination: second@example.test', { exact: true }).waitFor();
    await click('agree'); await see('The reviewed Lightning destination changed. Refresh and review again.');
    assert.equal(manualPayments[0].expected_destination, oldDestination.address);
    assert.equal(manualPayments[0].expected_confirmed_at_ms, oldDestination.confirmed_at_ms);
    assert.equal(records.filter((r) => r.kind === 'lightning').length, 0);
    await other.close();

    // Refresh and sign in again to view the same receipt in this tab.
    await refresh();
    await withdraw('2400'); await see('Failed to fetch');
    assert.ok(await byId('withdrawalRecovery').isVisible());
    assert.ok(await byId('withdraw').isHidden());
    const requestId = manualPayments.at(-1).request_id;
    assert.equal(manualPayments.at(-1).expected_confirmed_at_ms, destination.confirmed_at_ms);
    await refresh(); await see('2,400 sats to second@example.test · paid');
    assert.ok(await byId('recoverWithdrawal').isHidden());
    await page.reload(); await page.getByLabel('Till admin password').fill('test-only-review'); await submit('auth');
    await tab('Payouts').click();
    await see('2,400 sats to second@example.test · paid');
    assert.ok(await byId('withdraw').isHidden());
    assert.equal(manualPayments.filter((d) => d.request_id === requestId).length, 1);
    assert.equal(records.filter((r) => r.kind === 'lightning').length, 1);
    await click('newWithdrawal');
    assert.equal(await page.getByLabel('Amount in sats').inputValue(), '');
    assert.equal(await page.evaluate(() => history.state.tillWithdrawal), undefined);

    for (const width of [320, 390, 768, 1280]) {
      await page.setViewportSize({ width, height: 844 });
      for (const name of ['Overview', 'Payouts', 'Pairing']) {
        await tab(name).click();
        assert.equal(await page.getByRole('tabpanel').count(), 1);
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), name + ' overflow at ' + width);
        assert.ok((await tab(name).boundingBox()).height >= 44);
      }
    }
    await click('exit'); assert.equal(await byId('console').isVisible(), false);
    assert.equal(await byId('federation').textContent(), ''); assert.equal(await byId('destination').textContent(), '');
    assert.equal(await page.evaluate(() => localStorage.length + sessionStorage.length), 0);
    await page.getByLabel('Till admin password').fill('test-only-review'); await submit('auth'); await see('100,000 sats');
    assert.equal(await tab('Overview').getAttribute('aria-selected'), 'true');
    assert.deepEqual(errors, []);
    console.log('PASS login, federation join, sales, keyboard/touch tabs, default/override/fallback pairing, registry claim, pending address, payout modes, missing module/fees/funds/halt/changed destination, refused withdrawal, pre-commit retry idempotency, reviewed settings patches, reviewed Lightning destination, receipt recovery through refresh/reload, clean sign-out, local font, all three panels at four widths');
  } finally {
    await browser.close();
  }
})().catch((e) => { console.error(e); process.exitCode = 1; });
