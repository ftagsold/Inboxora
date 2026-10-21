import { describe, expect, it } from 'vitest';
import { safeImageType } from './imageResponse.js';

describe('safeImageType', () => {
  it('accepts raster types, ignoring parameters and case', () => {
    expect(safeImageType('image/PNG; charset=binary')).toBe('image/png');
    expect(safeImageType('image/jpg')).toBe('image/jpeg');
  });

  it.each(['image/svg+xml', 'text/html', '', null, undefined])('rejects %s', type => {
    expect(safeImageType(type)).toBeNull();
  });
});
