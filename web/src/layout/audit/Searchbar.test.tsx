import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { renderWithRouter } from '../../testUtils';
import Searchbar from './Searchbar';

describe('Searchbar', () => {
  beforeEach(() => {
    vi.spyOn(window, 'scrollTo').mockImplementation(() => {});
  });

  it('searches from the first page, keeping the organization and time range and clearing the filters', async () => {
    const { router, user } = renderSearchbar('/audit?kind=team-added&time_range=1w&organization=org2&page=3');

    await user.type(searchInput(), 'foo');
    await user.keyboard('{Enter}');

    expect(router.location.search).toBe('?ts_query_web=foo&time_range=1w&organization=org2&page=1');
  });

  it('removes the applied search when clearing it, keeping the organization and time range', async () => {
    const { router, user } = renderSearchbar('/audit?ts_query_web=foo&time_range=1w&organization=org2&page=3');

    await user.click(clearButton());

    expect(router.location.search).toBe('?time_range=1w&organization=org2&page=1');
    expect(searchInput()).toHaveValue('');
  });

  it('only empties the input when clearing text that was not searched yet', async () => {
    const initialSearch = '?ts_query_web=foo&organization=org2&page=3';
    const { router, user } = renderSearchbar(`/audit${initialSearch}`);

    await user.type(searchInput(), 'bar');
    await user.click(clearButton());

    expect(router.location.search).toBe(initialSearch);
    expect(searchInput()).toHaveValue('');
  });

  it('follows the search text in the URL', async () => {
    const { router } = renderSearchbar('/audit?ts_query_web=foo');

    await router.navigate('/audit?ts_query_web=bar');

    expect(searchInput()).toHaveValue('bar');
  });
});

const clearButton = (): HTMLElement => screen.getByRole('button', { name: 'Clear search' });

const renderSearchbar = (url: string) => {
  const user = userEvent.setup();
  return { router: renderWithRouter(<Searchbar />, [url]), user };
};

// The input has no accessible name, so the placeholder is what identifies it
const searchInput = (): HTMLInputElement => screen.getByPlaceholderText<HTMLInputElement>('Search changes');
