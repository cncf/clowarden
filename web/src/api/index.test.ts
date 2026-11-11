import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ErrorKind, SearchQuery, SortBy, SortDirection } from '../types';
import API from './index';

const BASE_QUERY: SearchQuery = {
  limit: 50,
  offset: 0,
  sort_by: SortBy.Date,
  sort_direction: SortDirection.DESC,
};

// Keep in sync with the query parsed by the search test in clowarden-server/src/handlers/tests.rs
const SEARCH_URL =
  '/audit/api/changes/search?limit=50&offset=100&sort_by=date&sort_direction=desc' +
  '&applied_from=2024-01-01%2010%3A00%3A00%2B0000&applied_to=2024-01-02%2010%3A00%3A00%2B0000' +
  '&organization=org1&ts_query_web=hello&service[0]=github&kind[0]=repository-added' +
  '&kind[1]=team-added&pr_number[0]=12&pr_merged_by[0]=user1';

describe('API', () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    fetchMock.mockReset();
    fetchMock.mockImplementation(async () => jsonResponse([]));
    vi.stubGlobal('fetch', fetchMock);
    vi.setSystemTime(new Date('2024-01-02T10:00:00Z'));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  describe('getOrganizations', () => {
    it('returns the organizations as a plain array', async () => {
      fetchMock.mockResolvedValue(jsonResponse(['org1', 'org2']));

      await expect(API.getOrganizations()).resolves.toEqual(['org1', 'org2']);
      expect(fetchMock).toHaveBeenCalledExactlyOnceWith('/audit/api/organizations', {});
    });
  });

  describe('searchChangesInput', () => {
    it('builds the URL parsed by the server', async () => {
      await API.searchChangesInput({
        filters: {
          service: ['github'],
          kind: ['repository-added', 'team-added'],
          pr_number: ['12'],
          pr_merged_by: ['user1'],
        },
        limit: 50,
        offset: 100,
        organization: 'org1',
        sort_by: SortBy.Date,
        sort_direction: SortDirection.DESC,
        time_range: '24h',
        ts_query_web: 'hello',
      });

      expect(requestedURL()).toBe(SEARCH_URL);
    });

    it('applies the requested sort direction', async () => {
      await API.searchChangesInput({ ...BASE_QUERY, sort_direction: SortDirection.ASC });

      expect(requestedParams().get('sort_direction')).toBe('asc');
    });

    it('uses the default sort and time range when they are omitted', async () => {
      await API.searchChangesInput({ limit: 50, offset: 0 } as SearchQuery);

      const params = requestedParams();
      expect(params.get('sort_by')).toBe('date');
      expect(params.get('sort_direction')).toBe('desc');
      expect(params.get('applied_from')).toBe('2023-12-02 10:00:00+0000');
      expect(params.get('applied_to')).toBe('2024-01-02 10:00:00+0000');
    });

    it('leaves out the organization, the text and the filters when they are not set', async () => {
      await API.searchChangesInput({ ...BASE_QUERY, filters: {} });

      expect(requestedURL()).not.toMatch(/organization|ts_query_web|\[0\]/);
    });

    it.each(['R&D', 'foo#1', 'c++', 'a=b', '100%', 'two words'])('sends the search text %s unchanged', async (text) => {
      await API.searchChangesInput({ ...BASE_QUERY, filters: { kind: ['team-added'] }, ts_query_web: text });

      const params = requestedParams();
      expect(params.get('ts_query_web')).toBe(text);
      expect(params.get('kind[0]')).toBe('team-added');
    });

    it('encodes the organization and the filter values', async () => {
      await API.searchChangesInput({
        ...BASE_QUERY,
        filters: { pr_merged_by: ['user&1', 'user#2'] },
        organization: 'org&1',
      });

      const params = requestedParams();
      expect(params.get('organization')).toBe('org&1');
      expect(params.get('pr_merged_by[0]')).toBe('user&1');
      expect(params.get('pr_merged_by[1]')).toBe('user#2');
    });

    it('wraps the changes and exposes the total count', async () => {
      const changes = [{ change_id: '1' }, { change_id: '2' }];
      fetchMock.mockResolvedValue(jsonResponse(changes, { 'Pagination-Total-Count': '25' }));

      await expect(API.searchChangesInput(BASE_QUERY)).resolves.toEqual({
        items: changes,
        'Pagination-Total-Count': '25',
      });
    });

    it('wraps an empty result', async () => {
      fetchMock.mockResolvedValue(jsonResponse([], { 'Pagination-Total-Count': '0' }));

      await expect(API.searchChangesInput(BASE_QUERY)).resolves.toEqual({
        items: [],
        'Pagination-Total-Count': '0',
      });
    });

    it('rejects with the message sent by the server', async () => {
      fetchMock.mockResolvedValue(jsonResponse({ message: 'boom' }, {}, 500));

      await expect(API.searchChangesInput(BASE_QUERY)).rejects.toEqual({ kind: ErrorKind.Other, message: 'boom' });
    });

    it('rejects without a message when the error body is not JSON', async () => {
      fetchMock.mockResolvedValue(new Response('invalid query', { status: 400 }));

      await expect(API.searchChangesInput(BASE_QUERY)).rejects.toEqual({ kind: ErrorKind.Other });
    });
  });

  // Helpers

  const requestedParams = (): URLSearchParams => new URL(requestedURL(), 'http://localhost').searchParams;

  const requestedURL = (): string => fetchMock.mock.calls[0][0];
});

const jsonResponse = (body: unknown, headers: { [key: string]: string } = {}, status: number = 200): Response =>
  new Response(JSON.stringify(body), { headers: { 'Content-Type': 'application/json', ...headers }, status });
