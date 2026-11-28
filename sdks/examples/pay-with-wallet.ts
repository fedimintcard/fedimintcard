import { TillClient, TillError, type TillOptions } from 'fedimintcard';

/** Persist these non-secret fields when the wallet creates its invoice, before the first tap. */
export interface WalletOrder {
  invoice: string;
  orderRef: string;
  maxFeeMsat: number;
}

/** Call with a live foreground link to this till and the PIN entered for this one tap. */
export async function payWalletOrder(pairing: string, options: TillOptions & Required<Pick<TillOptions, 'verifyInvoice'>>,
  order: WalletOrder, cardLink: string, pin: string) {
  const till = TillClient.fromPairing(pairing, options);
  try {
    const receipt = await till.payInvoice({ invoice: order.invoice, clientRef: order.orderRef,
      maxFeeMsat: order.maxFeeMsat, cardLink, pin });
    return { state: 'paid' as const, receipt };
  } catch (error) {
    if (error instanceof TillError && error.payId !== undefined &&
        (error.code === 'undecided' || error.code === 'pay_failed')) {
      // Persist payId with orderRef, including if this status request is interrupted.
      return { state: 'check_status' as const, payId: error.payId, orderRef: order.orderRef };
    }
    // A lost response leaves the order open. Preserve orderRef for reconciliation with the same card.
    throw error;
  }
}
