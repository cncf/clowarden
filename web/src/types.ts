export interface OutletContext {
  setInvisibleFooter: (value: boolean) => void;
}

export interface Prefs {
  search: { sort: { by: SortBy; direction: SortDirection } };
  theme: ThemePrefs;
}

export interface ThemePrefs {
  configured: string;
  effective: string;
}

export enum SortDirection {
  ASC = 'asc',
  DESC = 'desc',
}

export enum SortBy {
  Date = 'date',
}

export enum FilterKind {
  Service = 'service',
  Kind = 'kind',
  PRNumber = 'pr_number',
  PRMergedBy = 'pr_merged_by',
  AppliedSuccessfully = 'applied_successfully',
}

export enum Service {
  GitHub = 'github',
}

export enum ChangeKind {
  TeamAdded = 'team-added',
  TeamRemoved = 'team-removed',
  TeamMaintainerAdded = 'team-maintainer-added',
  TeamMaintainerRemoved = 'team-maintainer-removed',
  TeamMemberAdded = 'team-member-added',
  TeamMemberRemoved = 'team-member-removed',
  RepositoryAdded = 'repository-added',
  RepositoryTeamAdded = 'repository-team-added',
  RepositoryTeamRemoved = 'repository-team-removed',
  RepositoryTeamRoleUpdated = 'repository-team-role-updated',
  RepositoryCollaboratorAdded = 'repository-collaborator-added',
  RepositoryCollaboratorRemoved = 'repository-collaborator-removed',
  RepositoryCollaboratorRoleUpdated = 'repository-collaborator-role-updated',
  RepositoryVisibilityUpdated = 'repository-visibility-updated',
}

export interface SortOption {
  label: string;
  by: SortBy;
  direction: SortDirection;
}

export interface Option {
  label: string;
  value: string;
}

export interface Error {
  kind: ErrorKind;
  message?: string;
}

export enum ErrorKind {
  Other,
  NotFound,
}

export interface BasicQuery {
  ts_query_web?: string;
  time_range?: string;
  filters?: {
    [key: string]: string[];
  };
}

export interface SearchQuery extends BasicQuery {
  limit: number;
  offset: number;
  sort_by: SortBy;
  sort_direction: SortDirection;
}

export interface SearchFiltersURL extends BasicQuery {
  pageNumber: number;
}

export interface Change {
  change_id: string;
  service: string;
  kind: string;
  extra: {
    [key: string]: any;
  };
  applied_at: number;
  error?: string;
  reconciliation: ReconciliationStatus;
}

export interface ReconciliationStatus {
  reconciliation_id: string;
  completed_at: number;
  error?: string;
  pr_number: string;
  pr_created_by: string;
  pr_merged_by: string;
  pr_merged_at: string;
}

export interface SearchTipItem {
  content: JSX.Element | string;
  example: string;
}
