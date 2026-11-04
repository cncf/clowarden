import { describe, expect, it } from 'vitest';

import buildSearchParams from './buildSearchParams';
import prepareQueryString from './prepareQueryString';

describe('prepareQueryString', () => {
  it('always sets the page and omits empty values', () => {
    const queryString = prepareQueryString({
      filters: { kind: [] },
      pageNumber: 2,
      time_range: undefined,
      ts_query_web: '',
    });

    expect(queryString).toBe('?page=2');
  });

  it('does not write a null organization', () => {
    expect(prepareQueryString({ organization: null, pageNumber: 1 })).toBe('?page=1');
  });

  it('produces a query string that buildSearchParams reads back', () => {
    const query = {
      filters: { kind: ['team-added', 'team-removed'], pr_merged_by: ['user&1'] },
      organization: 'org 1',
      pageNumber: 4,
      time_range: '3d',
      ts_query_web: 'R&D #1',
    };

    expect(buildSearchParams(new URLSearchParams(prepareQueryString(query)))).toEqual(query);
  });
});
