import React from 'react';
import ReactDOM from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import App from './App.tsx';
import './index.css';
import './ui.css';
import './i18n.ts';
import './plugins/index.ts'; // register bundled plugins' UI slots before first paint
import { APP_NAME } from './appName.ts';

// The served title may still be the build placeholder (e.g. a page not processed by the
// container); the login screen and app chrome must show the configured name either way.
document.title = APP_NAME;

const rootElement = document.getElementById('root');

if (rootElement === null) {
  throw new Error('Unable to mount Inboxora: missing #root element.');
}

ReactDOM.createRoot(rootElement).render(
  <React.StrictMode>
    <BrowserRouter>
      <App />
    </BrowserRouter>
  </React.StrictMode>
);
