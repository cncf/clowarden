import { SelectedFilterBadge } from 'clo-ui/components/SelectedFilterBadge';
import isEmpty from 'lodash/isEmpty';
import isUndefined from 'lodash/isUndefined';
import { Fragment } from 'react';
import { IoMdCloseCircleOutline } from 'react-icons/io';

import { DATE_RANGE, FILTER_CATEGORY_NAMES } from '../../../data';
import { FilterKind, Option } from '../../../types';
import styles from './SelectedFilters.module.css';

interface Props {
  timeRange?: string;
  filters: { [key: string]: string[] };
  onChange: (name: string, value: string, checked: boolean) => void;
  onDateRangeChange: (timeRange?: string) => void;
}

const SelectedFilters = (props: Props) => {
  if (isEmpty(props.filters) && isUndefined(props.timeRange)) return null;

  const getFilterName = (type: FilterKind, filter: string): string => {
    switch (type) {
      case FilterKind.PRNumber:
        return `#${filter}`;
    }
    return filter;
  };

  const getTimeRangeName = (timeRange: string): string | null => {
    const selectedTimeRange = DATE_RANGE.find((d: Option) => d.value === timeRange);
    if (selectedTimeRange) {
      return selectedTimeRange.label;
    }
    return null;
  };

  return (
    <div className="d-none d-md-block mt-4">
      <div className="d-flex flex-row justify-content-start align-items-baseline">
        <div className="me-3">Filters:</div>
        <div role="list" className={`position-relative ${styles.badges}`}>
          {!isUndefined(props.timeRange) && (
            <span
              role="listitem"
              className={`badge bg-secondary rounded-0 text-light me-3 my-1 ${styles.badge} lightBorder`}
            >
              <div className="d-flex flex-row align-items-baseline">
                <div className={styles.content}>
                  <small className="text-uppercase fw-normal me-2">Time range:</small>
                  {getTimeRangeName(props.timeRange)}
                </div>
                <button
                  className={`btn btn-link btn-sm lh-1 ${styles.btn}`}
                  onClick={() => props.onDateRangeChange(undefined)}
                  aria-label="Remove date range filter"
                >
                  <IoMdCloseCircleOutline />
                </button>
              </div>
            </span>
          )}
          {Object.keys(props.filters).map((category: string) => {
            const categoryName = FILTER_CATEGORY_NAMES[category as FilterKind];
            return (
              <Fragment key={`filter_${category}`}>
                {props.filters[category].map((filter: string) => {
                  const filterName = getFilterName(category as FilterKind, filter);
                  return (
                    <Fragment key={`filter_${category}_${filter}`}>
                      <SelectedFilterBadge
                        categoryName={categoryName}
                        category={category}
                        filterName={filterName}
                        filter={filter}
                        onClick={() => props.onChange(category, filter as string, false)}
                      />
                    </Fragment>
                  );
                })}
              </Fragment>
            );
          })}
        </div>
      </div>
    </div>
  );
};

export default SelectedFilters;
