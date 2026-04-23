import { fail, ok } from '@/lib/http';
import { env } from '@/lib/env';
import { findPaymentForInvoice } from '@/lib/stellar';
import { markInvoiceExpired, markInvoicePaid, pendingInvoices } from '@/lib/data';

function authorized(request: Request) {
  const auth = request.headers.get('authorization');
  const bearer = auth?.replace('Bearer ', '');
  return bearer && bearer === env.cronSecret;
}

export async function GET(request: Request) {
  if (!authorized(request)) return fail('Unauthorized', 401);
  const invoices = await pendingInvoices();
  const results: Array<Record<string, unknown>> = [];

  for (const invoice of invoices) {
    if (Date.now() > new Date(invoice.expires_at).getTime()) {
      await markInvoiceExpired(invoice.id);
      results.push({ publicId: invoice.public_id, action: 'expired' });
      continue;
    }
    const payment = await findPaymentForInvoice(invoice);
    if (payment) {
      const payout = await markInvoicePaid({
        invoiceId: invoice.id,
        transactionHash: payment.hash,
        payload: payment.payment,
      });
      results.push({
        publicId: invoice.public_id,
        action: 'paid',
        txHash: payment.hash,
        payoutQueued: payout.payoutQueued,
        payoutSkipReason: payout.payoutSkipReason,
      });
    } else {
      results.push({ publicId: invoice.public_id, action: 'pending' });
    }
  }

  return ok({ scanned: invoices.length, results });
}
