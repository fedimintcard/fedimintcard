import {RegistryClient, RegistryStoreApi} from './api';

const post = (body: unknown, status = 200) =>
  ({ok: status < 400, status, json: async () => body, text: async () => ''} as unknown as Response);

test('the adapter signs in, opens a batch, and posts a row at the registry shapes', async () => {
  const seen: any[] = [];
  const origin = 'https://registry.test';
  const fetcher = async (url: string, init?: RequestInit) => {
    const path = String(url).replace(origin, '');
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    seen.push({path, body, auth: (init?.headers as any)?.Authorization ?? (init?.headers as any)?.authorization});
    if (path === '/auth/operator/session') return post({operator_id: 7, token: 'tok-1'});
    // the service wraps both answers: {batch: {...}} and {stored: {...}}
    if (path === '/registry/batches') return post({batch: {batch_id: 42, federation_id: 'f', count: 1, pin_policy: 'batch', load_sha256: 'x', created_at: 0}});
    if (path === '/registry/cards') return post({stored: {short_id: 'C1', received_at: 1}});
    throw new Error('unexpected ' + path);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  expect(api.serviceUrl).toBe(origin);
  const s = await api.signIn('dread', 'pw');
  expect(s).toEqual({operatorId: 7, token: 'tok-1'});
  // The invite scanned from a guardian UI on 2026-11-04 (federation.test.ts).
  const invite =
    'fed11qgqyj3mfwfhksw309uekyvesvenrzdpjx93xvdf4xcex2d33xajxycmzxf3nzdn9x9jxyctpxy6k2vnpv56nwvtpvsmxgepnx93kvvnzvscnjefn8qurxc3eqqqjp8wgs64nt3zuzc4j073a5mlr2cm4kp6t2jtv06d05kcu3elavy5g5h4kwl';
  await api.postBatch({federation: invite, count: 1, pinPolicy: 'batch'});
  await api.postCard({
    id: 'C1', at: 1000, phase: 'done', ok: true, pinPolicy: 'batch',
    pinState: 'set', notesIssued: [1048, 524], pending: false,
  } as any);
  // The registry records the federation's id, never the invite text.
  expect(seen[1].body.federation_id).toBe(
    '9dc886ab35c45c162b27fa3da6fe356375b074b5496c7e9afa5b1c8e7fd61288',
  );
  expect(seen[1].body.batch_id).toBeUndefined();
  expect(seen[2].body.batch_id).toBe(42);
  expect(seen[2].body.notes_issued).toBe(1572);
  expect(seen[2].body.pin_set).toBe(true);
});

test('a batch or a card before sign-in is refused, not sent', async () => {
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: 'https://r.test', fetcher: async () => { throw new Error('sent'); }}), 'https://r.test');
  await expect(api.postBatch({federation: 'f', count: 1, pinPolicy: 'batch'}))
    .rejects.toThrow(/before an operator signed in/);
});

test('a finished card carries its eight slot keys; sign-out revokes the token on the registry', async () => {
  const seen: {path: string; body: any; auth?: string}[] = [];
  const origin = 'https://registry.test';
  const fetcher = async (url: string, init?: RequestInit) => {
    const path = String(url).replace(origin, '');
    seen.push({
      path,
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
      auth: (init?.headers as any)?.Authorization ?? (init?.headers as any)?.authorization,
    });
    if (path === '/auth/operator/session') return post({operator_id: 1, token: 'op_tok'});
    if (path === '/registry/batches') return post({batch: {batch_id: 9}});
    if (path === '/registry/cards') return post({stored: {}});
    if (path === '/auth/operator/signout') return post({ok: true});
    throw new Error('unexpected ' + path);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  await api.signIn('dread', 'pw');
  await api.postBatch({federation: 'x', count: 1, pinPolicy: 'perCard'});
  const keys = Array.from({length: 8}, (_, i) => String(i).repeat(64));
  await api.postCard({
    id: '00000000', at: 1, phase: 'done', ok: true, pinPolicy: 'perCard',
    pinState: 'set', notesIssued: [], pending: false, slotKeys: keys,
  } as any);
  await api.postCard({
    id: 'unkeyed-1', at: 2, phase: 'probe', ok: false, stop: 'pulled', pinPolicy: 'perCard',
    pinState: 'unknown', notesIssued: [], pending: false,
  } as any);
  const cards = seen.filter(s => s.path === '/registry/cards');
  expect(cards[0]!.body.slot_pubkeys).toEqual(keys);
  expect('slot_pubkeys' in cards[1]!.body).toBe(false);

  await api.signOut();
  const out = seen.find(s => s.path === '/auth/operator/signout');
  expect(out?.auth).toBe('Bearer op_tok');
  await expect(api.postBatch({federation: 'x', count: 1, pinPolicy: 'batch'})).rejects.toThrow(/before an operator signed in/);
});

test('federation names come from the registry by decoded id; malformed rows are skipped', async () => {
  const origin = 'https://registry.test';
  const fed = '9d'.repeat(32);
  const fetcher = async (url: string) => {
    const path = String(url).replace(origin, '');
    if (path === '/auth/operator/session') return post({operator_id: 1, token: 'op_tok'});
    if (path === '/registry/federations') {
      return post({federations: [{federation_id: fed, name: 'Island Bitcoin'}, {federation_id: 7}]});
    }
    throw new Error('unexpected ' + path);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  await api.signIn('dread', 'pw');
  expect(await api.federationNames()).toEqual({[fed]: 'Island Bitcoin'});
});

test('O5: manager keys are checked before they reach a card; a reset and a card row carry the chip', async () => {
  const seen: {path: string; body: any}[] = [];
  const origin = 'https://registry.test';
  const chip = 'cplc-v1:4790d3213041093363205547';
  let keys: Record<string, unknown> = {chip, key_version: 1, enc: '0'.repeat(32), mac: '1'.repeat(32), dek: '2'.repeat(32)};
  const fetcher = async (url: string, init?: RequestInit) => {
    const path = String(url).replace(origin, '');
    seen.push({path, body: init?.body ? JSON.parse(String(init.body)) : undefined});
    if (path === '/auth/operator/session') return post({operator_id: 1, token: 'op_tok'});
    if (path === '/registry/manager-keys') return post({ok: true, ...keys});
    if (path === '/registry/cards/0a0b0c0d/reset-check') {
      return post({ok: true, short_id: '0a0b0c0d', chip, empty: false, resettable: false, reasons: ['1 live note, 1000 msat'],
        live_notes: 1, live_msat: 1000, pending_notes: 0, pending_msat: 0, open_charges: 0, open_loads: 0, replaced_by: null});
    }
    if (path === '/registry/cards/0a0b0c0d/reset') return post({ok: true, stranded: {live_notes: 1, pending_notes: 0}});
    if (path === '/registry/batches') return post({batch: {batch_id: 9}});
    if (path === '/registry/cards') return post({stored: {}});
    throw new Error('unexpected ' + path);
  };
  const api = new RegistryStoreApi(new RegistryClient({baseUrl: origin, fetcher}), origin);
  await api.signIn('dread', 'pw');
  expect(await api.managerKeys(chip, '0a0b0c0d')).toEqual(keys);
  expect(seen[1].body).toEqual({chip, short_id: '0a0b0c0d'});
  keys = {...keys, mac: '1'.repeat(30)};
  await expect(api.managerKeys(chip)).rejects.toThrow(/malformed/);
  keys = {...keys, mac: '1'.repeat(32), chip: 'cplc-v1:' + 'f'.repeat(24)};
  await expect(api.managerKeys(chip)).rejects.toThrow(/malformed/);

  const check = await api.resetCheck('0a0b0c0d');
  expect(check).toEqual(expect.objectContaining({resettable: false, live_notes: 1, reasons: ['1 live note, 1000 msat']}));

  const slotKeys = Array.from({length: 8}, (_, i) => String(i).repeat(64));
  const recorded = await api.recordReset('0a0b0c0d', {short_id: '1f2e3d4c', installed_at: 5, slot_pubkeys: slotKeys, chip});
  expect(recorded).toEqual({old_short_id: '0a0b0c0d', short_id: '1f2e3d4c', stranded_notes: 1});
  const body = seen.find(x => x.path.endsWith('/reset'))!.body;
  expect(body).toEqual(expect.objectContaining({short_id: '1f2e3d4c', pin_set: true, pin_policy: 'perCard', notes_issued: 0, chip}));
  expect(Object.keys(body).sort()).toEqual(
    ['chip', 'installed_at', 'load_sha256', 'notes_issued', 'pin_policy', 'pin_set', 'short_id', 'slot_pubkeys'],
  );

  await api.postBatch({federation: 'f', count: 1, pinPolicy: 'batch'});
  await api.postCard({id: '1f2e3d4c', at: 1, phase: 'done', ok: true, pinPolicy: 'batch', pinState: 'set', notesIssued: [], pending: true, slotKeys, chip} as any);
  expect(seen.at(-1)!.body.chip).toBe(chip);
});
