import React from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { I18nProvider } from './i18n/I18nProvider';
import { UnsavedChangesProvider } from './lib/unsavedChanges';
import { initTheme } from './lib/theme';
import './styles/app.css';

// #450: apply the stored light/dark choice before the first paint.
initTheme();

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <I18nProvider>
      <UnsavedChangesProvider>
        <App />
      </UnsavedChangesProvider>
    </I18nProvider>
  </React.StrictMode>
);
