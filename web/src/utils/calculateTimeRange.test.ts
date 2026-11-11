import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DATE_RANGE } from '../data';
import calculateTimeRange from './calculateTimeRange';

const EXPECTED_FROM: { [key: string]: string } = {
  '12h': '2024-03-15 00:00:00+0000',
  '1M': '2024-02-15 12:00:00+0000',
  '1h': '2024-03-15 11:00:00+0000',
  '1w': '2024-03-08 12:00:00+0000',
  '24h': '2024-03-14 12:00:00+0000',
  '3d': '2024-03-12 12:00:00+0000',
  '6h': '2024-03-15 06:00:00+0000',
};

describe('calculateTimeRange', () => {
  beforeEach(() => {
    vi.setSystemTime(new Date('2024-03-15T12:00:00Z'));
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it.each(DATE_RANGE)('covers the "$label" option', ({ value }) => {
    expect(calculateTimeRange(value)).toEqual({
      from: EXPECTED_FROM[value],
      to: '2024-03-15 12:00:00+0000',
    });
  });
});
