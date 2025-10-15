import { FilterSection } from 'clo-ui/components/FilterSection';
import { FiltersSection } from 'clo-ui/components/FiltersSection';
import { InputFiltersSection } from 'clo-ui/components/InputFiltersSection';
import isEmpty from 'lodash/isEmpty';
import isUndefined from 'lodash/isUndefined';
import React from 'react';
import { IoMdCloseCircleOutline } from 'react-icons/io';

import { FILTERS, SEARCHABLE_FILTERS } from '../../../data';
import { FilterKind } from '../../../types';
import styles from './Filters.module.css';
import OrganizationsSelect from './OrganizationsSelect';
import TimeRange from './TimeRange';

interface Props {
  organizations?: string[];
  selectedOrg?: string | null;
  onOrganizationChange: (org: string) => void;
  timeRange?: string;
  visibleTitle: boolean;
  activeFilters: {
    [key: string]: string[];
  };
  onChange: (name: string, value: string, checked: boolean, type?: string) => void;
  onDateRangeChange: (timeRange?: string) => void;
  onResetFilters?: () => void;
  device: string;
}

const Filters = (props: Props) => {
  return (
    <div className={styles.filters}>
      {props.visibleTitle && (
        <div className="d-flex flex-row align-items-center justify-content-between pb-2 mb-4 border-bottom">
          <div className="h6 text-uppercase mb-0 lh-base text-primary fw-bold">Filters</div>
          {(!isEmpty(props.activeFilters) || !isUndefined(props.timeRange)) && (
            <button className="btn btn-link text-primary" onClick={props.onResetFilters} aria-label="Reset filters">
              <div className="d-flex flex-row align-items-center">
                <IoMdCloseCircleOutline className="me-2" />

                <small>Reset</small>
              </div>
            </button>
          )}
        </div>
      )}

      <div className={`fw-bold text-uppercase text-primary ${styles.categoryTitle}`}>
        <small>Organization</small>
      </div>

      <OrganizationsSelect
        selectedOrg={props.selectedOrg}
        organizations={props.organizations}
        onOrganizationChange={props.onOrganizationChange}
      />

      <div className={`fw-bold text-uppercase text-primary ${styles.categoryTitle}`}>
        <small>Time range</small>
      </div>

      <TimeRange onDateRangeChange={props.onDateRangeChange} value={props.timeRange} />

      {FILTERS.map((section: FilterSection) => {
        const activeFilters = props.activeFilters[section.key!];
        return (
          <React.Fragment key={`sec_${section.key}`}>
            <FiltersSection
              device={props.device}
              activeFilters={activeFilters}
              section={section}
              onChange={props.onChange}
              visibleTitle
            />
          </React.Fragment>
        );
      })}

      {SEARCHABLE_FILTERS.map((section: FilterSection) => {
        const activeFilters = props.activeFilters[section.key!];
        return (
          <React.Fragment key={`sec_${section.key}`}>
            <InputFiltersSection
              device={props.device}
              activeFilters={activeFilters}
              section={section}
              inputType={section.key! === FilterKind.PRNumber ? 'number' : undefined}
              decoratorActiveFilter={section.key! === FilterKind.PRNumber ? '#' : undefined}
              onChange={props.onChange}
            />
          </React.Fragment>
        );
      })}
    </div>
  );
};

export default Filters;
