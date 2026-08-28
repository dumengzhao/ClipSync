import React from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import PullToast from './PullToast';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './styles.css';

const isToast = getCurrentWindow().label === 'pull-toast';

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    {isToast ? <PullToast /> : <App />}
  </React.StrictMode>,
);
