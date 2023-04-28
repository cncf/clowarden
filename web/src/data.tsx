import { FilterSection } from 'clo-ui';

import { ChangeKind, FilterKind, Option, SearchTipItem, Service, SortBy, SortDirection, SortOption } from './types';

export const DEFAULT_SORT_BY = SortBy.Date;
export const DEFAULT_SORT_DIRECTION = SortDirection.DESC;
export const DEFAULT_TIME_RANGE = '1M';
export const PAGINATION_LIMIT = 50;

export const SORT_OPTIONS: SortOption[] = [
  {
    label: 'Applied at (asc)',
    by: SortBy.Date,
    direction: SortDirection.ASC,
  },
  {
    label: 'Applied at (desc)',
    by: SortBy.Date,
    direction: SortDirection.DESC,
  },
];

export const FILTER_CATEGORY_NAMES = {
  [FilterKind.Service]: 'Service',
  [FilterKind.Kind]: 'Change',
  [FilterKind.PRNumber]: 'PR number',
  [FilterKind.PRMergedBy]: 'PR merged by',
  [FilterKind.AppliedSuccessfully]: 'Status',
};

export const FILTERS: FilterSection[] = [
  {
    key: FilterKind.Service,
    title: 'Services',
    options: [{ value: Service.GitHub, name: 'GitHub' }],
  },
  {
    key: FilterKind.Kind,
    title: 'Changes',
    options: {
      team: [
        { value: ChangeKind.TeamAdded, name: 'Added' },
        { value: ChangeKind.TeamRemoved, name: 'Removed' },
        { value: ChangeKind.TeamMaintainerAdded, name: 'Maintainer added' },
        { value: ChangeKind.TeamMaintainerRemoved, name: 'Maintainer removed' },
        { value: ChangeKind.TeamMemberAdded, name: 'Member added' },
        { value: ChangeKind.TeamMemberRemoved, name: 'Member removed' },
      ],
      repository: [
        { value: ChangeKind.RepositoryAdded, name: 'Added' },
        { value: ChangeKind.RepositoryTeamAdded, name: 'Team added' },
        { value: ChangeKind.RepositoryTeamRemoved, name: 'Team removed' },
        { value: ChangeKind.RepositoryTeamRoleUpdated, name: 'Team role updated' },
        { value: ChangeKind.RepositoryCollaboratorAdded, name: 'Collaborator added' },
        { value: ChangeKind.RepositoryCollaboratorRemoved, name: 'Collaborator removed' },
        { value: ChangeKind.RepositoryCollaboratorRoleUpdated, name: 'Collaborator role updated' },
        { value: ChangeKind.RepositoryVisibilityUpdated, name: 'Visibility updated' },
      ],
    },
  },
];

export const SEARCHABLE_FILTERS: FilterSection[] = [
  {
    key: FilterKind.PRNumber,
    placeholder: 'Add PR number',
    title: 'PR number',
    options: [],
  },
  {
    key: FilterKind.PRMergedBy,
    placeholder: 'Add GitHub handle',
    title: 'PR merged by',
    options: [],
  },
];

export const SEARCH_TIPS: SearchTipItem[] = [
  {
    content: (
      <>
        Use <span className="fw-semibold">multiple words</span> to refine your search.
      </>
    ),
    example: 'team1 repo1',
  },
];

export const DATE_RANGE: Option[] = [
  {
    label: 'Last hour',
    value: '1h',
  },
  {
    label: 'Last 6 hours',
    value: '6h',
  },
  {
    label: 'Last 12 hours',
    value: '12h',
  },
  {
    label: 'Last 24 hours',
    value: '24h',
  },
  {
    label: 'Last 3 days',
    value: '3d',
  },
  {
    label: 'Last week',
    value: '1w',
  },
  {
    label: 'Last month',
    value: '1M',
  },
];
