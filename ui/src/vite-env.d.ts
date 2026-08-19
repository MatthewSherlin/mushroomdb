/// <reference types="vite/client" />

interface Window {
  /** Dev-only. Production builds must not define this (e2e greps dist/). */
  __testHooks?: {
    glowScheduled: string[];
  };
}
