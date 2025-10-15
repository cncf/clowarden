import classNames from 'classnames';
import { ExternalLink } from 'clo-ui/components/ExternalLink';
import { useOutsideClick } from 'clo-ui/hooks/useOutsideClick';
import { RefObject, useRef, useState } from 'react';
import { BsList } from 'react-icons/bs';
import { FaGithub } from 'react-icons/fa';

import styles from './MobileSettings.module.css';
import ThemeMode from './ThemeMode';

const MobileSettings = () => {
  const [visibleDropdown, setVisibleDropdown] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);
  useOutsideClick([ref as unknown as RefObject<HTMLElement>], visibleDropdown, () => setVisibleDropdown(false));

  return (
    <div ref={ref} className="d-flex d-md-none ms-auto position-relative">
      <button
        className={`btn btn-sm btn-link text-white rounded-0 lh-1 fs-5 ms-3 ${styles.btn}`}
        type="button"
        onClick={() => setVisibleDropdown(!visibleDropdown)}
        aria-label="Mobile settings button"
        aria-expanded={visibleDropdown}
      >
        <BsList />
      </button>

      <div role="menu" className={classNames('dropdown-menu rounded-0', styles.dropdown, { show: visibleDropdown })}>
        <ThemeMode device="mobile" closeDropdown={() => setVisibleDropdown(false)} />

        <hr />

        <div className="dropdown-item mb-2">
          <ExternalLink
            className="text-decoration-none fw-bold d-inline-block w-100"
            label="Github link"
            href="https://github.com/cncf/clowarden"
          >
            <div className="d-flex flex-row align-items-center py-1">
              <FaGithub />
              <div className="ms-2">Github</div>
            </div>
          </ExternalLink>
        </div>
      </div>
    </div>
  );
};

export default MobileSettings;
