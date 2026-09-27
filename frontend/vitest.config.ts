import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    globals: false,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    css: false,
    // jsdom + axe page scans are CPU-bound and run slower on shared CI
    // runners than locally; the 5 s default caused spurious timeouts.
    testTimeout: 15_000
  }
});
