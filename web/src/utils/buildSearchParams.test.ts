import { describe, expect, it } from 'vitest';

import buildSearchParams from './buildSearchParams';

describe('buildSearchParams', () => {
  it('collects repeated filter keys into arrays', () => {
    const params = new URLSearchParams('kind=team-added&kind=team-removed&pr_number=12');

    expect(buildSearchParams(params).filters).toEqual({
      kind: ['team-added', 'team-removed'],
      pr_number: ['12'],
    });
  });

  it('drops keys that are not filters', () => {
    const params = new URLSearchParams('foo=bar&limit=100000&service=github');

    expect(buildSearchParams(params).filters).toEqual({ service: ['github'] });
  });

  it('reads the organization, the text, the time range and the page', () => {
    const params = new URLSearchParams('organization=org2&ts_query_web=foo&time_range=1w&page=3');

    expect(buildSearchParams(params)).toEqual({
      filters: {},
      organization: 'org2',
      pageNumber: 3,
      time_range: '1w',
      ts_query_web: 'foo',
    });
  });

  it('defaults to the first page and leaves absent values undefined', () => {
    expect(buildSearchParams(new URLSearchParams())).toEqual({
      filters: {},
      organization: undefined,
      pageNumber: 1,
      time_range: undefined,
      ts_query_web: undefined,
    });
  });
});
