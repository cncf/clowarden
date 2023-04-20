import { isNull } from 'lodash';
import { ChangeEvent, useRef } from 'react';

import { DATE_RANGE, DEFAULT_TIME_RANGE } from '../../../data';
import { Option } from '../../../types';

export interface Props {
  onDateRangeChange: (timeRange?: string) => void;
  value?: string;
}

const TimeRange = (props: Props) => {
  const selectEl = useRef<HTMLSelectElement>(null);

  const handleChange = (event: ChangeEvent<HTMLSelectElement>) => {
    props.onDateRangeChange(event.target.value);
    forceBlur();
  };

  const forceBlur = (): void => {
    if (!isNull(selectEl) && !isNull(selectEl.current)) {
      selectEl.current.blur();
    }
  };

  return (
    <div className="mt-2 mb-3">
      <select
        ref={selectEl}
        className="form-select form-select-sm rounded-0 cursorPointer"
        value={props.value || DEFAULT_TIME_RANGE}
        onChange={handleChange}
        aria-label="Date range select"
      >
        {DATE_RANGE.map((opt: Option) => (
          <option key={`date_${opt.value}`} value={opt.value}>
            {opt.label}
          </option>
        ))}
        ;
      </select>
    </div>
  );
};

export default TimeRange;
