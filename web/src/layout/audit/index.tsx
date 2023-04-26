import classNames from 'classnames';
import {
  ElementWithTooltip,
  Loading,
  NoData,
  Pagination,
  scrollToTop,
  Sidebar,
  SortOptions,
  SubNavbar,
  useBreakpointDetect,
} from 'clo-ui';
import { isEmpty, isUndefined } from 'lodash';
import moment from 'moment';
import { Fragment, useContext, useEffect, useState } from 'react';
import { AiFillCheckCircle, AiFillCloseCircle } from 'react-icons/ai';
import { FaFilter } from 'react-icons/fa';
import { IoMdCloseCircleOutline } from 'react-icons/io';
import { MdInfoOutline } from 'react-icons/md';
import { useNavigate, useSearchParams } from 'react-router-dom';

import API from '../../api';
import { AppContext, updateSort } from '../../context/AppContextProvider';
import { PAGINATION_LIMIT, SORT_OPTIONS } from '../../data';
import { Change, ChangeKind, SearchFiltersURL, SortBy, SortDirection } from '../../types';
import buildSearchParams from '../../utils/buildSearchParams';
import prepareQueryString from '../../utils/prepareQueryString';
import styles from './Audit.module.css';
import Filters from './filters/Filters';
import SelectedFilters from './filters/SelectedFilters';
import Searchbar from './Searchbar';

interface FiltersProp {
  [key: string]: string[];
}

const Audit = () => {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const { ctx, dispatch } = useContext(AppContext);
  const { sort } = ctx.prefs.search;
  const [text, setText] = useState<string | undefined>();
  const [isLoading, setIsLoading] = useState<boolean>(false);
  const [timeRange, setTimeRange] = useState<string | undefined>();
  const [filters, setFilters] = useState<FiltersProp>({});
  const [pageNumber, setPageNumber] = useState<number>(1);
  const [total, setTotal] = useState<number>(0);
  const [changes, setChanges] = useState<Change[] | null | undefined>();
  const [apiError, setApiError] = useState<string | null>(null);
  const [alternativeView, setAlternativeView] = useState<boolean>(false);
  const limit = PAGINATION_LIMIT;
  const filtersApplied = !isEmpty(filters);
  const point = useBreakpointDetect();

  const getCurrentFilters = (): SearchFiltersURL => {
    return {
      pageNumber: pageNumber,
      ts_query_web: text,
      time_range: timeRange,
      filters: filters,
    };
  };

  const onResetFilters = (): void => {
    navigate({
      pathname: '/audit/',
      search: prepareQueryString({
        pageNumber: 1,
        ts_query_web: text,
        filters: {},
      }),
    });
  };

  const onDateRangeChange = (timeRange?: string) => {
    navigate({
      pathname: '/audit/',
      search: prepareQueryString({
        ...getCurrentFilters(),
        time_range: timeRange,
        pageNumber: 1,
      }),
    });
  };

  const updateCurrentPage = (searchChanges: any) => {
    navigate({
      pathname: '/audit/',
      search: prepareQueryString({
        ...getCurrentFilters(),
        pageNumber: 1,
        ...searchChanges,
      }),
    });
  };

  const onPageNumberChange = (pageNumber: number): void => {
    updateCurrentPage({
      pageNumber: pageNumber,
    });
  };

  const onFiltersChange = (name: string, value: string, checked: boolean): void => {
    const currentFilters = filters || {};
    let newFilters = isUndefined(currentFilters[name]) ? [] : currentFilters[name].slice();
    if (checked) {
      newFilters.push(value);
    } else {
      newFilters = newFilters.filter((el) => el !== value);
    }

    updateCurrentPage({
      filters: { ...currentFilters, [name]: newFilters },
    });
  };

  const onSortChange = (value: string): void => {
    const opts = value.split('_');
    // Load pageNumber is forced before update Sorting criteria
    navigate(
      {
        pathname: '/audit/',
        search: prepareQueryString({
          ...getCurrentFilters(),
          pageNumber: 1,
        }),
      },
      { replace: true }
    );
    dispatch(updateSort(opts[0] as SortBy, opts[1] as SortDirection));
  };

  const calculateOffset = (pNumber: number): number => {
    return pNumber && limit ? (pNumber - 1) * limit : 0;
  };

  useEffect(() => {
    const formattedParams = buildSearchParams(searchParams);
    setTimeRange(formattedParams.time_range);
    setFilters(formattedParams.filters || {});
    setText(formattedParams.ts_query_web);
    setPageNumber(formattedParams.pageNumber);

    async function searchProjects() {
      setIsLoading(true);
      try {
        const newSearchResults = await API.searchChangesInput({
          ts_query_web: formattedParams.ts_query_web,
          time_range: formattedParams.time_range,
          sort_by: sort.by,
          sort_direction: sort.direction,
          filters: formattedParams.filters || {},
          limit: limit,
          offset: calculateOffset(formattedParams.pageNumber),
        });
        setTotal(parseInt(newSearchResults['Pagination-Total-Count']));
        setChanges(newSearchResults.items);
      } catch {
        // TODO - error
        setApiError('An error occurred searching changes.');
      } finally {
        setIsLoading(false);
        scrollToTop();
      }
    }
    searchProjects();
  }, [searchParams, sort.by, sort.direction]); /* eslint-disable-line react-hooks/exhaustive-deps */

  useEffect(() => {
    if (!isUndefined(point) && !['xl', 'xxl'].includes(point)) {
      setAlternativeView(true);
    } else {
      setAlternativeView(false);
    }
  }, [point]);

  return (
    <>
      {alternativeView ? (
        <>
          <NoData className={styles.extraMargin}>
            <p className="h5 mb-0">The view is not optimized yet for mobile devices.</p>
          </NoData>
        </>
      ) : (
        <>
          <SubNavbar>
            <div className="d-flex flex-column w-100">
              <div className="d-flex flex-column flex-sm-row align-items-center justify-content-between flex-nowrap">
                <div className="d-flex flex-row flex-md-column align-items-center align-items-md-start w-100 text-truncate">
                  <Sidebar
                    label="Filters"
                    className="d-inline-block d-md-none me-2"
                    wrapperClassName="d-inline-block px-4"
                    buttonType={`btn-primary btn-sm rounded-circle position-relative ${styles.btnMobileFilters}`}
                    buttonIcon={<FaFilter />}
                    closeButtonClassName={styles.closeSidebar}
                    closeButton={
                      <>
                        {isLoading ? (
                          <>
                            <Loading spinnerClassName={styles.spinner} noWrapper smallSize />
                            <span className="ms-2">Searching...</span>
                          </>
                        ) : (
                          <>See {total} changes</>
                        )}
                      </>
                    }
                    leftButton={
                      <>
                        {filtersApplied && (
                          <div className="d-flex align-items-center">
                            <IoMdCloseCircleOutline className={`text-dark ${styles.resetBtnDecorator}`} />
                            <button
                              className="btn btn-link btn-sm p-0 ps-1 text-dark"
                              onClick={onResetFilters}
                              aria-label="Reset filters"
                            >
                              Reset
                            </button>
                          </div>
                        )}
                      </>
                    }
                    header={<div className="h6 text-uppercase mb-0 flex-grow-1">Filters</div>}
                  >
                    <div role="menu">
                      <Filters
                        device="mobile"
                        timeRange={timeRange}
                        activeFilters={filters}
                        onChange={onFiltersChange}
                        onDateRangeChange={onDateRangeChange}
                        visibleTitle={false}
                      />
                    </div>
                  </Sidebar>
                  <div className={`text-truncate fw-bold w-100 ${styles.searchResults}`} role="status">
                    {total > 0 && (
                      <span className="pe-1">
                        {calculateOffset(pageNumber) + 1} - {total < limit * pageNumber ? total : limit * pageNumber}{' '}
                        <span className="ms-1">of</span>{' '}
                      </span>
                    )}
                    {total}
                    <span className="ps-1"> changes </span>
                  </div>
                </div>
                <div className="d-flex flex-wrap flex-row justify-content-sm-end mt-3 mt-sm-0 w-100">
                  <SortOptions
                    options={SORT_OPTIONS as any[]}
                    by={sort.by}
                    direction={sort.direction}
                    width={180}
                    onSortChange={onSortChange}
                  />
                </div>
              </div>

              <SelectedFilters
                timeRange={timeRange}
                filters={filters}
                onChange={onFiltersChange}
                onDateRangeChange={onDateRangeChange}
              />
            </div>
          </SubNavbar>

          <main role="main" className="container-lg flex-grow-1 mb-4 mb-md-5">
            {isLoading && <Loading className={styles.loading} position="fixed" transparentBg />}
            <div
              className={classNames('h-100 position-relative d-flex flex-row align-items-start', {
                'opacity-75': isLoading,
              })}
            >
              <aside
                className={`d-none d-md-block position-relative p-3 rounded-0 border border-1 mb-3 mb-lg-4 ${styles.sidebar}`}
                aria-label="Filters"
              >
                <Filters
                  device="desktop"
                  timeRange={timeRange}
                  activeFilters={filters}
                  onChange={onFiltersChange}
                  onDateRangeChange={onDateRangeChange}
                  onResetFilters={onResetFilters}
                  visibleTitle
                />
              </aside>
              <div className={`d-flex flex-column flex-grow-1 mt-2 mt-md-3 ${styles.contentWrapper}`}>
                {apiError && (
                  <NoData className={styles.extraMargin}>
                    <div className="mb-4 mb-lg-5 h2">{apiError}</div>
                    <p className="h5 mb-0">Please try again later.</p>
                  </NoData>
                )}

                {changes && (
                  <>
                    <Searchbar />
                    {isEmpty(changes) ? (
                      <NoData>
                        <div className="h4">
                          We're sorry!
                          <p className="h6 mb-0 mt-3 lh-base">
                            <span> We can't seem to find any changes that match your search </span>
                            {!isEmpty(filters) ? <span className="ps-1">with the selected filters</span> : <>.</>}
                          </p>
                          <p className="h6 mb-0 mt-5 lh-base">
                            You can{' '}
                            {!isEmpty(filters) ? (
                              <button
                                className="btn btn-link text-dark fw-bold py-0 pb-1 px-0"
                                onClick={onResetFilters}
                                aria-label="Reset filters"
                              >
                                <u>reset the filters</u>
                              </button>
                            ) : (
                              <button
                                className="btn btn-link text-dark fw-bold py-0 pb-1 px-0"
                                onClick={() => {
                                  navigate({
                                    pathname: '/audit/',
                                    search: prepareQueryString({
                                      pageNumber: 1,
                                      filters: {},
                                    }),
                                  });
                                }}
                                aria-label="Browse all packages"
                              >
                                <u>browse all changes</u>
                              </button>
                            )}
                            <> or try a new search.</>
                          </p>
                        </div>
                      </NoData>
                    ) : (
                      <div className={`ms-3 ${styles.list}`}>
                        <table
                          className={`table table-striped table-hover table-bordered table-md mb-0 ${styles.table}`}
                        >
                          <thead className={styles.tableHeader}>
                            <tr>
                              <th scope="col" className="text-center">
                                Service
                              </th>
                              <th scope="col" className="text-center">
                                Change
                              </th>
                              <th scope="col" className="text-center">
                                Applied at
                              </th>
                              <th scope="col" className="text-center">
                                PR
                              </th>
                              <th scope="col" className="text-center">
                                PR merged by
                              </th>
                              <th scope="col" className="text-center">
                                Status
                              </th>
                            </tr>
                          </thead>
                          <tbody className={styles.tableContent}>
                            {changes.map((change: Change) => {
                              return (
                                <Fragment key={`tr_${change.change_id}`}>
                                  <tr>
                                    <td className="text-center align-middle">{change.service}</td>
                                    <td className="align-middle">
                                      <div className="fw-semibold">{change.kind}</div>
                                      <div className="d-flex flex-row flex-nowrap align-items-center">
                                        {(() => {
                                          switch (change.kind) {
                                            case ChangeKind.TeamAdded:
                                              return (
                                                <>
                                                  <div className="text-truncate text-nowrap">
                                                    <small className="text-uppercase text-muted">Team:</small>{' '}
                                                    {change.extra.team.name}
                                                  </div>
                                                  <div className="d-none d-md-inline-block">
                                                    <ElementWithTooltip
                                                      tooltipClassName={styles.tooltipMessage}
                                                      className={`position-relative ms-1 ps-1 ${styles.tooltipIcon}`}
                                                      element={<MdInfoOutline />}
                                                      tooltipWidth={250}
                                                      tooltipMessage={
                                                        <div className={`text-start ${styles.tooltip}`}>
                                                          <div>
                                                            <span className="text-uppercase text-muted">Name:</span>{' '}
                                                            {change.extra.team.name}
                                                          </div>
                                                          <div>
                                                            <span className="text-uppercase text-muted">Members:</span>
                                                            {isUndefined(change.extra.team.members) ? (
                                                              <> -</>
                                                            ) : (
                                                              <>
                                                                {change.extra.team.members.length === 0 ? (
                                                                  <> []</>
                                                                ) : (
                                                                  <ul>
                                                                    {change.extra.team.members.map((m: string) => {
                                                                      return <li key={`member_${m}`}>{m}</li>;
                                                                    })}
                                                                  </ul>
                                                                )}
                                                              </>
                                                            )}
                                                          </div>
                                                          <div>
                                                            <span className="text-uppercase text-muted">
                                                              Maintainers:
                                                            </span>
                                                            {isUndefined(change.extra.team.maintainers) ? (
                                                              <> -</>
                                                            ) : (
                                                              <>
                                                                {change.extra.team.maintainers.length === 0 ? (
                                                                  <> []</>
                                                                ) : (
                                                                  <ul>
                                                                    {change.extra.team.maintainers.map((m: string) => {
                                                                      return <li key={`maintainer_${m}`}>{m}</li>;
                                                                    })}
                                                                  </ul>
                                                                )}
                                                              </>
                                                            )}
                                                          </div>
                                                        </div>
                                                      }
                                                      visibleTooltip
                                                      active
                                                    />
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.TeamRemoved:
                                              return (
                                                <div className="text-truncate text-nowrap">
                                                  <small className="text-uppercase text-muted">Team:</small>{' '}
                                                  {change.extra.team_name}
                                                </div>
                                              );
                                            case ChangeKind.TeamMaintainerAdded:
                                            case ChangeKind.TeamMaintainerRemoved:
                                            case ChangeKind.TeamMemberAdded:
                                            case ChangeKind.TeamMemberRemoved:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Team:</small>{' '}
                                                    {change.extra.team_name}
                                                  </div>
                                                  <div
                                                    className={`ms-3 text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">User:</small>{' '}
                                                    {change.extra.user_name}
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryAdded:
                                              return (
                                                <>
                                                  <div className="text-truncate text-nowrap">
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo.name}
                                                  </div>
                                                  <div className="d-none d-md-inline-block">
                                                    <ElementWithTooltip
                                                      tooltipClassName={styles.tooltipMessage}
                                                      className={`position-relative ms-1 ps-1 ${styles.tooltipIcon}`}
                                                      element={<MdInfoOutline />}
                                                      tooltipWidth={250}
                                                      tooltipMessage={
                                                        <div className={`text-start ${styles.tooltip}`}>
                                                          <div>
                                                            <span className="text-uppercase text-muted">Name:</span>{' '}
                                                            {change.extra.repo.name}
                                                          </div>
                                                          <div>
                                                            <span className="text-uppercase text-muted">
                                                              Visibility:
                                                            </span>{' '}
                                                            {change.extra.repo.visibility}
                                                          </div>
                                                          <div>
                                                            <span className="text-uppercase text-muted">
                                                              Collaborators:
                                                            </span>
                                                            <>
                                                              {isUndefined(change.extra.repo.collaborators) ? (
                                                                <>{' {}'}</>
                                                              ) : (
                                                                <>
                                                                  {isEmpty(
                                                                    Object.keys(change.extra.repo.collaborators)
                                                                  ) ? (
                                                                    <>{' {}'}</>
                                                                  ) : (
                                                                    <ul>
                                                                      {Object.keys(change.extra.repo.collaborators).map(
                                                                        (c: string) => {
                                                                          return (
                                                                            <li key={`collaborator_${c}`}>
                                                                              {c}: {change.extra.repo.collaborators[c]}
                                                                            </li>
                                                                          );
                                                                        }
                                                                      )}
                                                                    </ul>
                                                                  )}
                                                                </>
                                                              )}
                                                            </>
                                                          </div>
                                                          <div>
                                                            <span className="text-uppercase text-muted">Teams:</span>
                                                            {isUndefined(change.extra.repo.teams) ? (
                                                              <>{' {}'}</>
                                                            ) : (
                                                              <>
                                                                {isEmpty(Object.keys(change.extra.repo.teams)) ? (
                                                                  <>{' {}'}</>
                                                                ) : (
                                                                  <ul>
                                                                    {Object.keys(change.extra.repo.teams).map(
                                                                      (t: string) => {
                                                                        return (
                                                                          <li key={`team_${t}`}>
                                                                            {t}: {change.extra.repo.teams[t]}
                                                                          </li>
                                                                        );
                                                                      }
                                                                    )}
                                                                  </ul>
                                                                )}
                                                              </>
                                                            )}
                                                          </div>
                                                        </div>
                                                      }
                                                      visibleTooltip
                                                      active
                                                    />
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryTeamRemoved:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo_name}
                                                  </div>
                                                  <div
                                                    className={`ms-3 text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Team:</small>{' '}
                                                    {change.extra.team_name}
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryTeamAdded:
                                            case ChangeKind.RepositoryTeamRoleUpdated:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate3opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo_name}
                                                  </div>
                                                  <div
                                                    className={`ms-3 text-truncate text-nowrap ${styles.maxWidthTruncate3opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Team:</small>{' '}
                                                    {change.extra.team_name}
                                                  </div>
                                                  <div className="ms-3 text-nowrap">
                                                    <small className="text-uppercase text-muted">Role:</small>{' '}
                                                    {change.extra.role}
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryCollaboratorRemoved:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo_name}
                                                  </div>
                                                  <div
                                                    className={`ms-3 text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">User:</small>{' '}
                                                    {change.extra.user_name}
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryCollaboratorAdded:
                                            case ChangeKind.RepositoryCollaboratorRoleUpdated:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate3opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo_name}
                                                  </div>
                                                  <div
                                                    className={`ms-3 text-truncate text-nowrap ${styles.maxWidthTruncate3opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">User:</small>{' '}
                                                    {change.extra.user_name}
                                                  </div>
                                                  <div className="ms-3 text-nowrap">
                                                    <small className="text-uppercase text-muted">Role:</small>{' '}
                                                    {change.extra.role}
                                                  </div>
                                                </>
                                              );
                                            case ChangeKind.RepositoryVisibilityUpdated:
                                              return (
                                                <>
                                                  <div
                                                    className={`text-truncate text-nowrap ${styles.maxWidthTruncate2opts}`}
                                                  >
                                                    <small className="text-uppercase text-muted">Repo:</small>{' '}
                                                    {change.extra.repo_name}
                                                  </div>
                                                  <div className="ms-3 text-nowrap">
                                                    <small className="text-uppercase text-muted">Visibility:</small>{' '}
                                                    {change.extra.visibility}
                                                  </div>
                                                </>
                                              );
                                            default:
                                              return <></>;
                                          }
                                        })()}
                                      </div>
                                    </td>
                                    <td className="text-center align-middle">
                                      {moment.unix(change.applied_at).format('L LT')}
                                    </td>
                                    <td className="text-center align-middle">
                                      {!isUndefined(change.reconciliation.pr_number)
                                        ? `#${change.reconciliation.pr_number}`
                                        : '-'}
                                    </td>
                                    <td className="text-center align-middle">
                                      <div className="mw-100 text-truncate">
                                        {change.reconciliation.pr_merged_by || '-'}
                                      </div>
                                    </td>
                                    <td className="text-center align-middle fs-5">
                                      {isUndefined(change.error) ? (
                                        <AiFillCheckCircle className="text-success" />
                                      ) : (
                                        <AiFillCloseCircle className="text-danger" />
                                      )}
                                    </td>
                                  </tr>
                                </Fragment>
                              );
                            })}
                          </tbody>
                        </table>
                      </div>
                    )}
                  </>
                )}

                <div className="mt-auto mx-auto">
                  <Pagination
                    limit={limit}
                    offset={0}
                    total={total}
                    active={pageNumber}
                    className="mt-4 mt-md-5 mb-0 mb-md-2"
                    onChange={onPageNumberChange}
                  />
                </div>
              </div>
            </div>
          </main>
        </>
      )}
    </>
  );
};

export default Audit;
