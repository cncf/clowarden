import moment, { DurationInputArg1, DurationInputArg2 } from 'moment';

export interface TimeRangeData {
  from: string;
  to: string;
}

const calculateTimeRange = (timeRange: string): TimeRangeData => {
  const amount = timeRange.substring(0, timeRange.length - 1);
  const unit = timeRange.slice(-1);

  return {
    from: moment()
      .subtract(amount as DurationInputArg1, unit as DurationInputArg2)
      .format('YYYY-MM-DD HH:mm:ssZZ'),
    to: moment().format('YYYY-MM-DD HH:mm:ssZZ'),
  };
};

export default calculateTimeRange;
