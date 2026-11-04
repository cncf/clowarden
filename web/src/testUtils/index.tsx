import { act, render } from '@testing-library/react';
import { ReactNode } from 'react';
import { Location, MemoryRouter, NavigationType, useLocation, useNavigate, useNavigationType } from 'react-router-dom';

export interface RouterState {
  location: Location;
  navigate: (to: string | number) => Promise<void>;
  navigationType: NavigationType;
}

/** Renders a React tree in a memory router and exposes the router state. */
export const renderWithRouter = (ui: ReactNode, initialEntries: string[], initialIndex?: number): RouterState => {
  const router = {} as RouterState;
  const RouterProbe = () => {
    const navigate = useNavigate();
    router.location = useLocation();
    router.navigationType = useNavigationType();
    router.navigate = (to) =>
      act(async () => {
        await (typeof to === 'number' ? navigate(to) : navigate(to));
      });
    return null;
  };

  render(
    <MemoryRouter initialEntries={initialEntries} initialIndex={initialIndex}>
      <RouterProbe />
      {ui}
    </MemoryRouter>
  );
  return router;
};
