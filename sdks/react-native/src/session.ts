import type { Transceive } from 'fedimintcard';

export interface NfcSessionOptions {
  /** iOS NFC sheet text. */
  prompt?: string;
  /** Close an outstanding scan or end the session when the caller leaves its screen. */
  signal?: AbortSignal;
}

/** Native operations are passed in separately so the same lifetime rules cover both platforms. */
export interface NativeSession {
  start(): Promise<void>;
  open(prompt: string): Promise<void>;
  configure(): Promise<void>;
  exchange(command: Uint8Array): Promise<Uint8Array>;
  close(): Promise<void>;
}

export function makeSession(native: NativeSession) {
  let busy = false;
  return async function withNfcTransport<T>(
    run: (transceive: Transceive) => Promise<T>,
    options: NfcSessionOptions = {},
  ): Promise<T> {
    if (busy) throw new Error('An NFC session is already open');
    busy = true;
    let requested = false;
    let usable = false;
    let transmitting = false;
    let closing: Promise<void> | undefined;
    function checkAbort() {
      if (options.signal?.aborted) throw new Error('NFC session cancelled');
    }
    function close() {
      usable = false;
      if (requested) closing ??= native.close();
      return closing;
    }
    const onAbort = () => { void close()?.catch(() => {}); };
    try {
      checkAbort();
      options.signal?.addEventListener('abort', onAbort, { once: true });
      await native.start();
      requested = true;
      checkAbort();
      await native.open(options.prompt ?? 'Hold your Fedimint Card near the phone.');
      checkAbort();
      await native.configure();
      checkAbort();
      usable = true;
      const transceive: Transceive = async (command) => {
        if (!usable) throw new Error('This NFC session has ended');
        if (transmitting) throw new Error('Another card command is in progress');
        transmitting = true;
        try {
          const response = await native.exchange(command);
          checkAbort();
          return response;
        } finally {
          transmitting = false;
        }
      };
      const result = await run(transceive);
      checkAbort();
      return result;
    } finally {
      options.signal?.removeEventListener('abort', onAbort);
      try {
        await close();
      } finally {
        busy = false;
      }
    }
  };
}
