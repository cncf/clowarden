import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DEFAULT_SORT_BY, DEFAULT_SORT_DIRECTION } from '../data';
import { Prefs, SortBy, SortDirection } from '../types';
import { LocalStoragePreferences } from './localStoragePreferences';

const LS_ITEM = 'clowardenPrefs';

describe('LocalStoragePreferences', () => {
  let storage: Pick<Storage, 'getItem' | 'setItem'>;

  beforeEach(() => {
    // Recent Node releases shadow the jsdom storage with an undefined global
    const items = new Map<string, string>();
    storage = {
      getItem: (key) => items.get(key) ?? null,
      setItem: (key, value) => {
        items.set(key, value);
      },
    };
    vi.stubGlobal('localStorage', storage);
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('falls back to the defaults when the stored value is malformed', () => {
    storage.setItem(LS_ITEM, '{not json');

    expect(new LocalStoragePreferences().getPrefs().search.sort).toEqual({
      by: DEFAULT_SORT_BY,
      direction: DEFAULT_SORT_DIRECTION,
    });
  });

  it('stores the defaults when the storage is empty', () => {
    const prefs = new LocalStoragePreferences().getPrefs();

    expect(JSON.parse(storage.getItem(LS_ITEM)!)).toEqual({ guest: prefs });
  });

  it('replaces an empty stored object with guest defaults', () => {
    storage.setItem(LS_ITEM, '{}');

    const prefs = new LocalStoragePreferences().getPrefs();

    expect(prefs.search.sort.by).toBe(DEFAULT_SORT_BY);
    expect(JSON.parse(storage.getItem(LS_ITEM)!)).toEqual({ guest: prefs });
  });

  it('keeps the preferences in memory when the storage cannot be written', () => {
    const preferences = new LocalStoragePreferences();
    const updatedPrefs: Prefs = {
      ...preferences.getPrefs(),
      search: { sort: { by: SortBy.Date, direction: SortDirection.ASC } },
    };
    vi.spyOn(storage, 'setItem').mockImplementation(() => {
      throw new Error('quota exceeded');
    });

    preferences.setPrefs(updatedPrefs);

    expect(preferences.getPrefs()).toEqual(updatedPrefs);
  });
});
