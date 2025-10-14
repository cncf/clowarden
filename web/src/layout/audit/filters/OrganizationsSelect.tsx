import isNull from 'lodash/isNull';
import isUndefined from 'lodash/isUndefined';
import { ChangeEvent, useRef } from 'react';

import styles from './OrganizationsSelect.module.css';

interface Props {
  organizations?: string[];
  selectedOrg?: string | null;
  onOrganizationChange: (org: string) => void;
}

const OrganizationsSelect = (props: Props) => {
  const select = useRef<HTMLSelectElement>(null);

  const forceBlur = (): void => {
    if (!isNull(select) && !isNull(select.current)) {
      select.current.blur();
    }
  };

  return (
    <div className="position-relative mt-2 mb-3">
      <select
        ref={select}
        className="form-select form-select-sm rounded-0 cursorPointer"
        aria-label="org-select"
        value={props.selectedOrg || ''}
        onChange={(e: ChangeEvent<HTMLSelectElement>) => {
          props.onOrganizationChange(e.target.value);
          forceBlur();
        }}
      >
        {!isUndefined(props.organizations) && (
          <>
            {props.organizations.map((org: string) => (
              <option key={`org_${org}`} value={org}>
                {org}
              </option>
            ))}
          </>
        )}
      </select>
      {isUndefined(props.selectedOrg) && (
        <div className={`position-absolute ${styles.loadingWrapper}`}>
          <span className="spinner-border text-primary spinner-border-sm" role="status" aria-hidden="true" />
        </div>
      )}
    </div>
  );
};

export default OrganizationsSelect;
