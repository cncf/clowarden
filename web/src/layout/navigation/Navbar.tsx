import { ExternalLink, Navbar as NavbarWrapper, scrollToTop } from 'clo-ui';
import { FaGithub } from 'react-icons/fa';
import { Link } from 'react-router-dom';

import logo from '../../media/clowarden.svg';
import MobileSettings from './MobileSettings';
import styles from './Navbar.module.css';
import Settings from './Settings';

const Navbar = () => {
  return (
    <NavbarWrapper navbarClassname={styles.navbar}>
      <>
        <div className={`me-0 me-md-4 mt-2 mt-md-0 ${styles.line}`}>
          <div className="d-flex flex-row align-items-start">
            <div className="position-relative">
              <Link to="/" onClick={() => scrollToTop()} className="cursorPointer">
                <img className={styles.logo} alt="CLOWarden logo" src={logo} />
              </Link>
              <div
                className={`position-relative badge rounded-0 text-uppercase fw-bold me-2 me-sm-3 ms-2 ${styles.alpha}`}
              >
                Alpha
              </div>
            </div>

            <MobileSettings />
          </div>
        </div>

        <div className="d-none d-md-flex flex-row align-items-center ms-auto">
          <Link
            to="/audit"
            className={`position-relative mx-4 text-light text-uppercase fw-bold text-decoration-none ${styles.link} navbarLink`}
          >
            Audit
          </Link>
          <ExternalLink
            className={`btn btn-md text-light fs-5 ${styles.ghLink}`}
            label="Github link"
            href="https://github.com/cncf/clowarden"
          >
            <FaGithub />
          </ExternalLink>
          <Settings />
        </div>
      </>
    </NavbarWrapper>
  );
};

export default Navbar;
