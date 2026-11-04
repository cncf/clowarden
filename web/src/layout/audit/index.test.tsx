import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent, { UserEvent } from '@testing-library/user-event';
import { ReactNode, useReducer } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import searchChanges from '../../__fixtures__/searchChanges.json';
import API from '../../api';
import { AppContext, appReducer } from '../../context/AppContextProvider';
import { DEFAULT_SORT_BY, DEFAULT_SORT_DIRECTION } from '../../data';
import { renderWithRouter } from '../../testUtils';
import { Change, ChangeKind, Prefs, SortBy, SortDirection } from '../../types';
import Audit from './index';

vi.mock('../../api', () => ({
  default: {
    getOrganizations: vi.fn(),
    searchChangesInput: vi.fn(),
  },
}));

type SearchResults = Awaited<ReturnType<typeof API.searchChangesInput>>;

const CHANGE_COLUMN = 1;
const CHANGES = searchChanges as unknown as Change[];

const DEFAULT_PREFS: Prefs = {
  search: { sort: { by: DEFAULT_SORT_BY, direction: DEFAULT_SORT_DIRECTION } },
  theme: { configured: 'light', effective: 'light' },
};

const EXPECTED_ROWS: [number, string[]][] = [
  [0, ['team-added', 'Team: team1']],
  [1, ['team-added', 'Team: team2']],
  [2, ['team-removed', 'Team: team3']],
  [3, ['team-maintainer-added', 'Team: team4', 'User: user4']],
  [4, ['team-maintainer-removed', 'Team: team5', 'User: user5']],
  [5, ['team-member-added', 'Team: team6', 'User: user6']],
  [6, ['team-member-removed', 'Team: team7', 'User: user7']],
  [7, ['repository-added', 'Repo: repo1']],
  [8, ['repository-added', 'Repo: repo2']],
  [9, ['repository-team-added', 'Repo: repo3', 'Team: team9', 'Role: maintain']],
  [10, ['repository-team-removed', 'Repo: repo4', 'Team: team10']],
  [11, ['repository-team-role-updated', 'Repo: repo5', 'Team: team11', 'Role: triage']],
  [12, ['repository-collaborator-added', 'Repo: repo6', 'User: user9', 'Role: read']],
  [13, ['repository-collaborator-removed', 'Repo: repo7', 'User: user10']],
  [14, ['repository-collaborator-role-updated', 'Repo: repo8', 'User: user11', 'Role: write']],
  [15, ['repository-visibility-updated', 'Repo: repo9', 'Visibility: internal']],
];

const LOADING_TEXT = 'Loading...';
const NO_ORGANIZATIONS_MESSAGE = /There are no organizations registered/;
const ORGANIZATIONS = ['org1', 'org2'];
const ORGANIZATIONS_ERROR = /Something went wrong fetching the organizations/;
const SEARCH_ERROR = /Something went wrong fetching the changes/;
const STATUS_COLUMN = 5;

// clo-ui shows the tooltip after 500ms, half of the default findBy timeout
const TOOLTIP_TIMEOUT = 3000;

describe('Audit', () => {
  beforeEach(() => {
    window.innerWidth = 1280;
    vi.spyOn(window, 'scrollTo').mockImplementation(() => {});
    vi.mocked(API.getOrganizations).mockResolvedValue(ORGANIZATIONS);
    vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults(CHANGES));
  });

  afterEach(() => {
    vi.resetAllMocks();
  });

  describe('deep links', () => {
    it('runs a single search with the state in the URL', async () => {
      vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults(CHANGES, 200));

      await renderLoadedAudit(
        '/audit?organization=org2&kind=team-added&kind=team-removed&pr_number=12&time_range=1w&ts_query_web=foo&page=3'
      );

      expect(API.searchChangesInput).toHaveBeenCalledExactlyOnceWith({
        filters: { kind: ['team-added', 'team-removed'], pr_number: ['12'] },
        limit: 50,
        offset: 100,
        organization: 'org2',
        sort_by: SortBy.Date,
        sort_direction: SortDirection.DESC,
        time_range: '1w',
        ts_query_web: 'foo',
      });
      expect(resultsSummary()).toHaveTextContent('101 - 150 of 200 changes');
    });

    it('uses the sort stored in the preferences', async () => {
      const prefs = { ...DEFAULT_PREFS, search: { sort: { by: SortBy.Date, direction: SortDirection.ASC } } };

      await renderLoadedAudit('/audit', prefs);

      expect(lastSearch().sort_direction).toBe(SortDirection.ASC);
    });

    it('searches the first organization when the URL has none', async () => {
      await renderLoadedAudit();

      expect(API.searchChangesInput).toHaveBeenCalledOnce();
      expect(lastSearch().organization).toBe('org1');
      expect(organizationSelect()).toHaveValue('org1');
    });

    it('does not search until the organizations are loaded', async () => {
      vi.mocked(API.getOrganizations).mockReturnValue(new Promise(() => {}));

      renderAudit('/audit?organization=org2');

      expect(API.searchChangesInput).not.toHaveBeenCalled();
    });

    it('does not search when there are no organizations', async () => {
      vi.mocked(API.getOrganizations).mockResolvedValue([]);

      renderAudit('/audit?organization=org2');

      await screen.findByText(NO_ORGANIZATIONS_MESSAGE);
      expect(API.searchChangesInput).not.toHaveBeenCalled();
    });
  });

  describe('URL state', () => {
    const INITIAL_URL = '/audit?kind=team-added&ts_query_web=foo&time_range=1w&organization=org2&page=3';

    beforeEach(() => {
      vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults(CHANGES, 400));
    });

    it('resets the page when a filter is added', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.click(filterCheckbox('team-removed'));

      expect(router.location.search).toBe(
        '?kind=team-added&kind=team-removed&ts_query_web=foo&time_range=1w&organization=org2&page=1'
      );
    });

    it('drops the filter key when its last value is removed', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.click(filterCheckbox('team-added'));

      expect(router.location.search).toBe('?ts_query_web=foo&time_range=1w&organization=org2&page=1');
    });

    it('resets the page when the time range changes', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.selectOptions(dateRangeSelect(), '3d');

      expect(router.location.search).toBe('?kind=team-added&ts_query_web=foo&time_range=3d&organization=org2&page=1');
    });

    it('replaces the history entry and resets the page when the organization changes', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.selectOptions(organizationSelect(), 'org1');

      expect(router.location.search).toBe('?kind=team-added&ts_query_web=foo&time_range=1w&organization=org1&page=1');
      expect(router.navigationType).toBe('REPLACE');
      expect(lastSearch().organization).toBe('org1');
    });

    it('resets the page and searches again when the sort changes', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.selectOptions(screen.getByRole('combobox', { name: 'Sort options select' }), 'date_asc');

      expect(router.location.search).toBe('?kind=team-added&ts_query_web=foo&time_range=1w&organization=org2&page=1');
      expect(router.navigationType).toBe('REPLACE');
      expect(lastSearch()).toMatchObject({ offset: 0, sort_direction: SortDirection.ASC });
    });

    it('keeps the filters and the organization when the page changes', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      // The next page button shares its label with the numbered one
      const pagination = await screen.findByRole('navigation', { name: 'pagination' });
      await user.click(within(pagination).getByText('4'));

      expect(router.location.search).toBe('?kind=team-added&ts_query_web=foo&time_range=1w&organization=org2&page=4');
      expect(lastSearch().offset).toBe(150);
    });

    it('keeps the organization and the text when the filters are reset', async () => {
      const { router, user } = await renderLoadedAudit(INITIAL_URL);

      await user.click(resetFiltersButton());

      expect(router.location.search).toBe('?ts_query_web=foo&organization=org2&page=1');
    });

    it('keeps the organization and the time range when browsing all changes', async () => {
      vi.mocked(API.searchChangesInput).mockResolvedValueOnce(searchResults([]));
      const { router, user } = renderAudit('/audit?ts_query_web=foo&time_range=1w&organization=org2&page=3');

      await user.click(await screen.findByRole('button', { name: 'Browse all changes' }));

      expect(router.location.search).toBe('?time_range=1w&organization=org2&page=1');
    });

    it.each([
      ['/audit?organization=org1&page=1', 'org1', 'org2'],
      ['/audit', 'org1', 'org2'],
      ['/audit?organization=org2&page=1', 'org2', 'org1'],
    ])(
      'follows the organization in the URL when going back to %s',
      async (previousUrl, expectedOrganization, laterOrganization) => {
        const laterUrl = `/audit?organization=${expectedOrganization}&page=2`;
        const { router, user } = await renderLoadedAudit([previousUrl, laterUrl], DEFAULT_PREFS, 1);
        await user.selectOptions(organizationSelect(), laterOrganization);

        await router.navigate(-1);

        expect(organizationSelect()).toHaveValue(expectedOrganization);
        expect(lastSearch().organization).toBe(expectedOrganization);
      }
    );

    it('does not write a null organization after failing to load the organizations', async () => {
      vi.mocked(API.getOrganizations).mockRejectedValue(new Error('organizations failed'));
      const { router, user } = renderAudit();
      await screen.findByText(ORGANIZATIONS_ERROR);

      await user.selectOptions(dateRangeSelect(), '3d');

      expect(router.location.search).toBe('?time_range=3d&page=1');
    });

    it.each<[string, () => Promise<string[]>, RegExp]>([
      ['fail to load', () => Promise.reject(new Error('organizations failed')), ORGANIZATIONS_ERROR],
      ['are empty', () => Promise.resolve([]), NO_ORGANIZATIONS_MESSAGE],
    ])('keeps the organization in the URL when the organizations %s', async (_, loadOrganizations, message) => {
      vi.mocked(API.getOrganizations).mockImplementation(loadOrganizations);
      const { router, user } = renderAudit('/audit?organization=org2');
      await screen.findByText(message);

      await user.selectOptions(dateRangeSelect(), '3d');

      expect(router.location.search).toBe('?time_range=3d&organization=org2&page=1');
    });

    it('keeps the organization in the URL when the organizations are still loading', async () => {
      vi.mocked(API.getOrganizations).mockReturnValue(new Promise(() => {}));
      const { router, user } = renderAudit('/audit?organization=org2');

      await user.selectOptions(dateRangeSelect(), '3d');

      expect(router.location.search).toBe('?time_range=3d&organization=org2&page=1');
    });

    it('keeps the organization in the URL when resetting the filters without organizations', async () => {
      vi.mocked(API.getOrganizations).mockRejectedValue(new Error('organizations failed'));
      const { router, user } = renderAudit('/audit?kind=team-added&organization=org2');
      await screen.findByText(ORGANIZATIONS_ERROR);

      await user.click(resetFiltersButton());

      expect(router.location.search).toBe('?organization=org2&page=1');
    });
  });

  describe('loading, empty and error states', () => {
    it('shows an error and does not search when the organizations fail to load', async () => {
      vi.mocked(API.getOrganizations).mockRejectedValue(new Error('organizations failed'));

      renderAudit();

      await screen.findByText(ORGANIZATIONS_ERROR);
      expect(mainContent()).toHaveTextContent('Please try again later.');
      expect(API.searchChangesInput).not.toHaveBeenCalled();
      expect(changesTable()).toBeNull();
    });

    it('reports that there are no organizations without suggesting a retry', async () => {
      vi.mocked(API.getOrganizations).mockResolvedValue([]);

      renderAudit();

      await screen.findByText(NO_ORGANIZATIONS_MESSAGE);
      expect(mainContent()).not.toHaveTextContent('Please try again later.');
      expect(changesTable()).toBeNull();
    });

    it('shows an error and no changes when the search fails', async () => {
      vi.mocked(API.searchChangesInput).mockRejectedValue(new Error('search failed'));

      renderAudit();

      await screen.findByText(SEARCH_ERROR);
      expect(resultsSummary()).toHaveTextContent(/^0 changes$/);
      expect(changesTable()).toBeNull();
    });

    it('removes the error when a later search succeeds', async () => {
      vi.mocked(API.searchChangesInput).mockRejectedValueOnce(new Error('search failed'));
      const { user } = renderAudit();
      await screen.findByText(SEARCH_ERROR);

      await user.click(filterCheckbox('team-added'));

      await screen.findByRole('table');
      expect(mainContent()).not.toHaveTextContent('Something went wrong');
    });

    it('offers browsing all changes when a search finds nothing', async () => {
      vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults([]));

      renderAudit('/audit?ts_query_web=foo&organization=org1&page=1');

      await screen.findByRole('button', { name: 'Browse all changes' });
      expect(mainContent()).toHaveTextContent("We can't seem to find any changes that match your search");
      expect(changesTable()).toBeNull();
    });

    it('only shows the error when a search fails', async () => {
      vi.mocked(API.searchChangesInput).mockRejectedValue(new Error('search failed'));

      renderAudit('/audit?ts_query_web=foo&organization=org1&page=1');

      await screen.findByText(SEARCH_ERROR);
      expect(mainContent()).not.toHaveTextContent("We can't seem to find any changes");
      expect(screen.queryByRole('button', { name: 'Browse all changes' })).toBeNull();
    });

    it('reports an empty result without showing the table', async () => {
      vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults([]));

      renderAudit();

      await screen.findByText('No changes recorded yet for this selection.');
      expect(changesTable()).toBeNull();
    });

    it.each(['resolve', 'reject'] as const)('hides the loading indicator once the search %ss', async (outcome) => {
      const search = deferred<SearchResults>();
      vi.mocked(API.searchChangesInput).mockReturnValue(search.promise);
      renderAudit();

      // Verify the indicator is visible while the search is pending
      await waitForSearches(1);
      await within(mainContent()).findByText(LOADING_TEXT);

      // Complete the search
      await act(async () => {
        if (outcome === 'resolve') {
          search.resolve(searchResults(CHANGES));
        } else {
          search.reject(new Error('search failed'));
        }
      });

      expect(loadingIndicator()).toBeNull();
    });
  });

  describe('stale searches', () => {
    const ORG1_CHANGES = CHANGES.slice(0, 1);
    const ORG2_CHANGES = CHANGES.slice(2, 3);

    const startSearches = async () => {
      const org1Search = deferred<SearchResults>();
      const org2Search = deferred<SearchResults>();
      vi.mocked(API.searchChangesInput).mockReturnValueOnce(org1Search.promise).mockReturnValueOnce(org2Search.promise);
      const { user } = renderAudit('/audit?organization=org1');
      await waitForSearches(1);
      await user.selectOptions(organizationSelect(), 'org2');
      await waitForSearches(2);
      return { org1Search, org2Search };
    };

    it('ignores a response arriving after a newer one', async () => {
      const { org1Search, org2Search } = await startSearches();

      await act(async () => org2Search.resolve(searchResults(ORG2_CHANGES, 1)));
      await act(async () => org1Search.resolve(searchResults(ORG1_CHANGES, 77)));

      expect(changesTable()).toHaveTextContent('team-removed');
      expect(changesTable()).not.toHaveTextContent('team-added');
      expect(resultsSummary()).toHaveTextContent('1 - 1 of 1 changes');
    });

    it('ignores a failure arriving after a newer response', async () => {
      const { org1Search, org2Search } = await startSearches();

      await act(async () => org2Search.resolve(searchResults(ORG2_CHANGES, 1)));
      await act(async () => org1Search.reject(new Error('search failed')));

      expect(mainContent()).not.toHaveTextContent('Something went wrong');
      expect(changesTable()).toHaveTextContent('team-removed');
    });

    it('keeps loading when an older response arrives before the newest one', async () => {
      const { org1Search, org2Search } = await startSearches();

      // Complete the older search while the newest one is pending
      await act(async () => org1Search.resolve(searchResults(ORG1_CHANGES, 77)));

      expect(loadingIndicator()).toBeInTheDocument();
      expect(changesTable()).toBeNull();

      // Complete the newest search
      await act(async () => org2Search.resolve(searchResults(ORG2_CHANGES, 1)));

      expect(loadingIndicator()).toBeNull();
      expect(changesTable()).toHaveTextContent('team-removed');
    });
  });

  describe('changes table', () => {
    it('covers every change kind in the fixture', () => {
      const fixtureKinds = new Set(CHANGES.map((change) => change.kind));

      expect([...fixtureKinds].sort()).toEqual(Object.values(ChangeKind).sort());
    });

    it.each(EXPECTED_ROWS)('renders the details of fixture change %i', async (index, expectedTexts) => {
      await renderLoadedAudit();

      const changeCell = rowCells(index)[CHANGE_COLUMN];
      expectedTexts.forEach((text) => expect(changeCell).toHaveTextContent(text));
    });

    it('renders the service, date and pull request of a change', async () => {
      await renderLoadedAudit();

      const cells = rowCells(0).map((cell) => cell.textContent);
      expect(cells[0]).toBe('github');
      expect(cells[2]).toBe('11/14/2023 10:13 PM');
      expect(cells[3]).toBe('#12');
      expect(cells[4]).toBe('user2');
    });

    it('renders a placeholder for a change without a pull request', async () => {
      await renderLoadedAudit();

      const cells = rowCells(2);
      expect(cells[3]).toHaveTextContent(/^-$/);
      expect(cells[4]).toHaveTextContent(/^-$/);
    });

    it('lists the members and maintainers of an added team', async () => {
      const { user } = await renderLoadedAudit();

      const tooltip = await openTooltip(user, 0);

      expect(tooltip).toHaveTextContent('Name: team1');
      expect(tooltip).toHaveTextContent('Members:user2user3');
      expect(tooltip).toHaveTextContent('Maintainers:user1');
    });

    it('renders an added team without members or maintainers', async () => {
      const { user } = await renderLoadedAudit();

      const tooltip = await openTooltip(user, 1);

      expect(tooltip).toHaveTextContent('Members: -');
      expect(tooltip).toHaveTextContent('Maintainers: -');
    });

    it('lists the collaborators and teams of an added repository', async () => {
      const { user } = await renderLoadedAudit();

      const tooltip = await openTooltip(user, 7);

      expect(tooltip).toHaveTextContent('Visibility: private');
      expect(tooltip).toHaveTextContent('Collaborators:user8: admin');
      expect(tooltip).toHaveTextContent('Teams:team8: write');
    });

    it('renders an added repository without collaborators or teams', async () => {
      const { user } = await renderLoadedAudit();

      const tooltip = await openTooltip(user, 8);

      expect(tooltip).toHaveTextContent('Collaborators: {}');
      expect(tooltip).toHaveTextContent('Teams: {}');
    });
  });

  describe('change status', () => {
    it('marks only changes without an error as applied', async () => {
      await renderLoadedAudit();

      const lastRow = CHANGES.length - 1;
      expect(successIcon(0)).toBeInTheDocument();
      expect(failureIcon(0)).toBeNull();
      expect(successIcon(lastRow)).toBeNull();
      expect(failureIcon(lastRow)).toBeInTheDocument();
    });

    it('shows the error of a failed change on hover', async () => {
      const { user } = await renderLoadedAudit();

      // Verify the error is hidden from the accessibility tree before hovering
      const lastRow = within(tableRow(CHANGES.length - 1));
      expect(lastRow.queryByRole('complementary')).toBeNull();

      await user.hover(lastRow.getByRole('button', { name: 'Toggle dropdown' }));

      expect(await lastRow.findByRole('complementary')).toHaveTextContent('visibility update failed');
    });

    it('relies on search_changes stripping null errors', async () => {
      // A null error is only absent because the query uses json_strip_nulls
      const change = { ...CHANGES[0], error: null } as unknown as Change;
      vi.mocked(API.searchChangesInput).mockResolvedValue(searchResults([change]));

      await renderLoadedAudit();

      expect(failureIcon(0)).toBeInTheDocument();
    });
  });
});

// Helpers

const AuditWithContext = (props: { children: ReactNode; prefs: Prefs }) => {
  const [ctx, dispatch] = useReducer(appReducer, { prefs: props.prefs });
  return <AppContext.Provider value={{ ctx, dispatch }}>{props.children}</AppContext.Provider>;
};

const changesTable = (): HTMLElement | null => screen.queryByRole('table');

const dateRangeSelect = (): HTMLElement => within(mainContent()).getByRole('combobox', { name: 'Date range select' });

const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, reject, resolve };
};

// The status icons have no accessible name, so their color is what identifies them
const failureIcon = (row: number): Element | null => statusCell(row).querySelector('svg.text-danger');

// Two filters are labelled "Added", so the value is what identifies a filter
const filterCheckbox = (value: string): HTMLInputElement =>
  within(mainContent())
    .getAllByRole<HTMLInputElement>('checkbox')
    .find((checkbox) => checkbox.value === value)!;

const lastSearch = () => vi.mocked(API.searchChangesInput).mock.lastCall![0];

const loadingIndicator = (): HTMLElement | null => within(mainContent()).queryByText(LOADING_TEXT);

const mainContent = (): HTMLElement => screen.getByRole('main');

// The info icon has no accessible name, so the clo-ui test id is what identifies it
const openTooltip = async (user: UserEvent, row: number): Promise<HTMLElement> => {
  const changeCell = rowCells(row)[CHANGE_COLUMN];
  await user.hover(within(changeCell).getByTestId('elementWithTooltip'));
  return within(changeCell).findByRole('tooltip', {}, { timeout: TOOLTIP_TIMEOUT });
};

const organizationSelect = (): HTMLElement => within(mainContent()).getByRole('combobox', { name: 'org-select' });

const renderAudit = (urls: string | string[] = '/audit', prefs: Prefs = DEFAULT_PREFS, initialIndex?: number) => {
  const user = userEvent.setup();
  const router = renderWithRouter(
    <AuditWithContext prefs={prefs}>
      <Audit />
    </AuditWithContext>,
    Array.isArray(urls) ? urls : [urls],
    initialIndex
  );
  return { router, user };
};

const renderLoadedAudit = async (...args: Parameters<typeof renderAudit>) => {
  const rendered = renderAudit(...args);
  await screen.findByRole('table');
  return rendered;
};

const resetFiltersButton = (): HTMLElement => within(mainContent()).getByRole('button', { name: 'Reset filters' });

const resultsSummary = (): HTMLElement =>
  screen.getAllByRole('status').find((status) => status.textContent?.includes('changes'))!;

const rowCells = (row: number): HTMLElement[] => within(tableRow(row)).getAllByRole('cell');

const searchResults = (items: Change[], total: number = items.length): SearchResults => ({
  items,
  'Pagination-Total-Count': total.toString(),
});

const statusCell = (row: number): HTMLElement => rowCells(row)[STATUS_COLUMN];

const successIcon = (row: number): Element | null => statusCell(row).querySelector('svg.text-success');

// The first row of the table is the header
const tableRow = (index: number): HTMLElement => within(changesTable()!).getAllByRole('row')[index + 1];

const waitForSearches = (count: number): Promise<void> =>
  waitFor(() => expect(API.searchChangesInput).toHaveBeenCalledTimes(count));
