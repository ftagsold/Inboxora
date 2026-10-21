import { describe, expect, it } from 'vitest';
import { clampIntervalSeconds } from './davSourceUrl.js';

describe('clampIntervalSeconds', () => {
  it('defaults a missing or invalid interval to the smallest allowed, 15 s', () => {
    for (const value of [undefined, null, '', 'soon', 0]) expect(clampIntervalSeconds(value)).toBe(15);
  });

  it('keeps a chosen interval and clamps it to 15–86400 s', () => {
    expect(clampIntervalSeconds(3600)).toBe(3600);
    expect(clampIntervalSeconds('300')).toBe(300);
    expect(clampIntervalSeconds(5)).toBe(15);
    expect(clampIntervalSeconds(999999)).toBe(86400);
  });
});
