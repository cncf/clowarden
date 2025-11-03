import type { Duration } from 'date-fns';
import { format, sub } from 'date-fns';

export interface TimeRangeData {
  from: string;
  to: string;
}

const calculateTimeRange = (timeRange: string): TimeRangeData => {
  const amount = Number.parseInt(timeRange.substring(0, timeRange.length - 1), 10);
  const unit = timeRange.slice(-1);
  const now = new Date();

  if (Number.isNaN(amount)) {
    const formattedNow = format(now, 'yyyy-MM-dd HH:mm:ssxxxx');
    return {
      from: formattedNow,
      to: formattedNow,
    };
  }

  const duration: Duration = {};

  switch (unit) {
    case 'h':
      duration.hours = amount;
      break;
    case 'd':
      duration.days = amount;
      break;
    case 'w':
      duration.weeks = amount;
      break;
    case 'M':
      duration.months = amount;
      break;
    default:
      duration.hours = amount;
      break;
  }

  const from = sub(now, duration);

  return {
    from: format(from, 'yyyy-MM-dd HH:mm:ssxxxx'),
    to: format(now, 'yyyy-MM-dd HH:mm:ssxxxx'),
  };
};

export default calculateTimeRange;
