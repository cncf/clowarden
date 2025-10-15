import isArray from 'lodash/isArray';
import isEmpty from 'lodash/isEmpty';
import isNull from 'lodash/isNull';
import isUndefined from 'lodash/isUndefined';

import { DEFAULT_SORT_BY, DEFAULT_SORT_DIRECTION, DEFAULT_TIME_RANGE } from '../data';
import { Change, Error, ErrorKind, SearchQuery } from '../types';
import calculateTimeRange from '../utils/calculateTimeRange';

interface FetchOptions {
  method: 'POST' | 'GET' | 'PUT' | 'DELETE' | 'HEAD';
  headers?: {
    [key: string]: string;
  };
  body?: string;
}

interface APIFetchProps {
  url: string;
  opts?: FetchOptions;
  headers?: string[];
}

class API_CLASS {
  private API_BASE_URL = '/audit/api';
  private HEADERS = {
    pagination: 'Pagination-Total-Count',
  };

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private getHeadersValue(res: any, params?: string[]): any {
    if (!isUndefined(params) && params.length > 0) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const headers: any = {};
      params.forEach((param: string) => {
        if (res.headers.has(param)) {
          headers[param] = res.headers.get(param);
        }
      });
      return headers;
    }
    return null;
  }

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private async processFetchOptions(opts?: FetchOptions): Promise<FetchOptions | any> {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const options: FetchOptions | any = opts || {};
    if (opts && ['DELETE', 'POST', 'PUT'].includes(opts.method)) {
      return {
        ...options,
        headers: {
          ...options.headers,
        },
      };
    }
    return options;
  }

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private async handleErrors(res: any) {
    if (!res.ok) {
      let error: Error;
      switch (res.status) {
        default:
          try {
            const text = await res.json();
            error = {
              kind: ErrorKind.Other,
              message: text.message !== '' ? text.message : undefined,
            };
          } catch {
            error = {
              kind: ErrorKind.Other,
            };
          }
      }
      throw error;
    }
    return res;
  }

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private async handleContent(res: any, headers?: string[]) {
    const response = res;
    let content;
    let tmpHeaders;

    switch (response.headers.get('Content-Type')) {
      case 'text/plain; charset=utf-8':
      case 'csv':
        content = await response.text();
        return content;
      case 'application/json':
        content = await response.json();
        tmpHeaders = this.getHeadersValue(res, headers);
        if (!isNull(tmpHeaders)) {
          if (isArray(content)) {
            content = { items: content };
          }
          content = { ...content, ...tmpHeaders };
        }
        return content;
      default:
        return response;
    }
  }

  private async apiFetch(props: APIFetchProps) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const options: FetchOptions | any = await this.processFetchOptions(props.opts);

    return fetch(props.url, options)
      .then(this.handleErrors)
      .then((res) => this.handleContent(res, props.headers))
      .catch((error) => Promise.reject(error));
  }

  public getOrganizations(): Promise<string[]> {
    return this.apiFetch({
      url: `${this.API_BASE_URL}/organizations`,
    });
  }

  public searchChangesInput(query: SearchQuery): Promise<{ items: Change[]; 'Pagination-Total-Count': string }> {
    let q: string = `limit=${query.limit}&offset=${query.offset}&sort_by=${
      query.sort_by || DEFAULT_SORT_BY
    }&sort_direction=${query.sort_direction || DEFAULT_SORT_DIRECTION}`;

    const timeRange = calculateTimeRange(query.time_range || DEFAULT_TIME_RANGE);
    q += `&applied_from=${encodeURIComponent(timeRange.from)}&applied_to=${encodeURIComponent(timeRange.to)}`;

    if (query.organization) {
      q += `&organization=${query.organization}`;
    }

    if (query.ts_query_web) {
      q += `&ts_query_web=${query.ts_query_web}`;
    }

    if (!isUndefined(query.filters) && !isEmpty(query.filters)) {
      Object.keys(query.filters!).forEach((k: string) => {
        query.filters![k].forEach((f: string, index: number) => {
          q += `&${k}[${index}]=${f}`;
        });
      });
    }
    return this.apiFetch({
      url: `${this.API_BASE_URL}/changes/search?${q}`,
      headers: [this.HEADERS.pagination],
      opts: {
        method: 'GET',
        headers: {
          'Content-Type': 'application/json',
        },
      },
    });
  }
}

const API = new API_CLASS();
export default API;
