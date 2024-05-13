import isNull from 'lodash/isNull';

import { FilterKind, SearchFiltersURL } from '../types';

interface F {
  [key: string]: string[];
}

const WHITELISTED_FILTER_KEYS = [
  FilterKind.Service,
  FilterKind.Kind,
  FilterKind.PRNumber,
  FilterKind.PRMergedBy,
  FilterKind.AppliedSuccessfully,
];

const buildSearchParams = (p: URLSearchParams): SearchFiltersURL => {
  const filters: F = {};

  p.forEach((value, key) => {
    if (WHITELISTED_FILTER_KEYS.includes(key as FilterKind)) {
      const values = filters[key] || [];
      values.push(value);
      filters[key] = values;
    }
  });

  return {
    ts_query_web: p.has('ts_query_web') ? p.get('ts_query_web')! : undefined,
    time_range: p.has('time_range') ? p.get('time_range')! : undefined,
    filters: { ...filters },
    pageNumber: p.has('page') && !isNull(p.get('page')) ? parseInt(p.get('page')!) : 1,
  };
};

export default buildSearchParams;
