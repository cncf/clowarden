import 'clo-ui/dist/styles/default.scss';
import './App.module.css';

import { BrowserRouter as Router, Navigate, Route, Routes } from 'react-router-dom';

import { AppContextProvider } from './context/AppContextProvider';
import Layout from './layout';
import Audit from './layout/audit';
import NotFound from './layout/notFound';

function App() {
  return (
    <AppContextProvider>
      <Router>
        <Routes>
          <Route path="/" element={<Layout />}>
            <Route index element={<Navigate to="/audit" replace />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="*" element={<NotFound />} />
          </Route>
        </Routes>
      </Router>
    </AppContextProvider>
  );
}

export default App;
