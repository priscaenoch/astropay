import { describe, expect, it } from 'vitest';
import { resolveCheckoutFailure } from '@/lib/paymentFailure';

describe('resolveCheckoutFailure', () => {
  it('maps Freighter not connected to install guidance', () => {
    const v = resolveCheckoutFailure('Freighter is not connected in this browser.', 'wallet');
    expect(v.title).toMatch(/Freighter/);
    expect(v.hints.some((h) => /extension/i.test(h))).toBe(true);
    expect(v.technical).toBeTruthy();
  });

  it('maps user rejection to cancellation copy', () => {
    const v = resolveCheckoutFailure('User declined signing', 'wallet');
    expect(v.title).toMatch(/cancel/);
    expect(v.hints.some((h) => /Pay now/i.test(h))).toBe(true);
  });

  it('maps underfunded Horizon errors', () => {
    const v = resolveCheckoutFailure('op_underfunded', 'submit');
    expect(v.title).toMatch(/USDC|XLM/);
    expect(v.hints.length).toBeGreaterThan(0);
  });

  it('maps invoice not found', () => {
    const v = resolveCheckoutFailure('Invoice not found', 'build');
    expect(v.title).toMatch(/Invoice/);
    expect(v.hints.some((h) => /merchant/i.test(h))).toBe(true);
  });

  it('returns stage-appropriate fallback for unknown strings', () => {
    const v = resolveCheckoutFailure('Some cryptic xyz', 'submit');
    expect(v.title).toMatch(/Payment|through|network/i);
    expect(v.description.length).toBeGreaterThan(10);
    expect(v.technical).toContain('cryptic');
  });
});
