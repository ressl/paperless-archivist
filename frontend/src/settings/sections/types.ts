import type { OllamaInstalledModel, RuntimeSettings } from '../../api/client';

// Shared structural types for the decomposed Settings sections. Kept in one
// place so every section file agrees on the same shapes without re-importing
// the monolith.

export type ModelCapability = 'text' | 'vision';

export type ModelProviderDescriptor = Pick<
  RuntimeSettings['ai']['providers'][number],
  'name' | 'kind' | 'base_url'
>;

export type OllamaModelLoadState = {
  loading: boolean;
  loaded: boolean;
  models: OllamaInstalledModel[];
  error: string | null;
};

export type ConnectionTestState = {
  status: 'idle' | 'running' | 'success' | 'error';
  title: string;
  description: string;
  hints: string[];
  details?: string;
};
