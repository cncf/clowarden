import { scrollToTop, Searchbar as SearchbarForm } from 'clo-ui';
import { useEffect, useState } from 'react';
// import { FaRegQuestionCircle } from 'react-icons/fa';
import { useNavigate, useSearchParams } from 'react-router-dom';

import prepareQueryString from '../../utils/prepareQueryString';
import styles from './Searchbar.module.css';
// import SearchTipsModal from './SearchTipsModal';

const Searchbar = () => {
  // const [openTips, setOpenTips] = useState<boolean>(false);
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const [value, setValue] = useState<string>('');
  const [currentSearch, setCurrentSearch] = useState<string | null>(null);

  useEffect(() => {
    const text = searchParams.get('ts_query_web');
    setValue(text || '');
    setCurrentSearch(text);
  }, [searchParams]);

  const search = () => {
    scrollToTop();
    navigate({
      pathname: '/audit/',
      search: prepareQueryString({
        pageNumber: 1,
        ts_query_web: value,
        filters: {},
      }),
    });
  };

  const cleanSearchValue = () => {
    if (currentSearch === value) {
      scrollToTop();
      navigate({
        pathname: '/audit/',
        search: prepareQueryString({
          pageNumber: 1,
          ts_query_web: '',
          filters: {},
        }),
      });
    } else {
      setValue('');
    }
  };

  return (
    <div className={`d-flex flex-row w-50 my-3 mx-auto position-relative ${styles.wrapper}`}>
      <SearchbarForm
        value={value}
        onValueChange={(newValue: string) => setValue(newValue)}
        onSearch={search}
        cleanSearchValue={cleanSearchValue}
        placeholder="Search changes"
        classNameWrapper="w-100"
        classNameSearch={`w-100 ${styles.search}`}
        bigSize={false}
      />
      {/* <div className={`d-none d-sm-inline-block ${styles.questionMark}`}>
        <button className="btn btn-link text-decoration-none" onClick={() => setOpenTips(!openTips)}>
          <FaRegQuestionCircle className={`position-absolute ${styles.question}`} />
        </button>
      </div>
      <div>
        <SearchTipsModal openTips={openTips} setOpenTips={setOpenTips} />
      </div> */}
    </div>
  );
};

export default Searchbar;
