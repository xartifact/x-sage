/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Optional absolute API base; empty when Vite proxies /api (dev) or same-origin (prod). */
  readonly VITE_API_BASE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
