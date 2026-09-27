import type { components, paths } from './schema'

type IsNever<Value> = [Value] extends [never] ? true : false
type HasOperation<
  Path extends keyof paths,
  Method extends keyof paths[Path],
> = IsNever<NonNullable<paths[Path][Method]>> extends true ? false : true
type ExpectAll<Checks extends readonly true[]> = Checks
type WebhookRequest = components['schemas']['WebhookConsumedRequest']
type UnblockRequest = components['schemas']['UnblockJobsRequest']
type HeaderOf<Operation> = Operation extends {
  parameters: { header?: infer Header }
}
  ? NonNullable<Header>
  : never
type RequiredKeys<Value> = {
  [Key in keyof Value]-?: {} extends Pick<Value, Key> ? never : Key
}[keyof Value]
type HasConditionalCsrf<Operation> = 'X-CSRF-Token' extends keyof HeaderOf<Operation>
  ? 'X-CSRF-Token' extends RequiredKeys<HeaderOf<Operation>>
    ? false
    : true
  : false
type HasRequiredCsrf<Operation> = 'X-CSRF-Token' extends RequiredKeys<
  HeaderOf<Operation>
>
  ? true
  : false
type HasJsonBodyRejections<Operation> = Operation extends {
  responses: infer Responses
}
  ? 400 | 413 | 415 | 422 extends keyof Responses
    ? true
    : false
  : false

/**
 * Compile-time contract for the routes added by issue #354. The generated
 * OpenAPI client must expose every runtime operation; `pnpm typecheck` fails
 * if regeneration drops a path or changes its HTTP method.
 */
export type GeneratedRouteContract = ExpectAll<[
  HasOperation<'/api/prompts/experiments', 'get'>,
  HasOperation<'/api/inventory/duplicates', 'get'>,
  HasOperation<'/api/batches/rerun', 'post'>,
  HasOperation<'/api/batches/rerun-failed', 'post'>,
  HasOperation<'/api/reviews/auto-fix-preview', 'post'>,
  HasOperation<'/api/reviews/auto-fix', 'post'>,
  HasOperation<'/api/reviews/{id}/auto-fix', 'post'>,
  HasOperation<'/api/operations/unblock-jobs', 'post'>,
  HasOperation<'/api/operations/provider-cooldowns', 'get'>,
  HasOperation<'/api/operations/provider-cooldowns/clear', 'post'>,
  HasOperation<'/api/operations/release-scheduled-retries', 'post'>,
  HasOperation<'/api/webhooks/paperless/document-consumed', 'post'>,
  {} extends WebhookRequest ? false : true,
  {} extends UnblockRequest ? true : false,
  HasConditionalCsrf<paths['/api/batches/rerun']['post']>,
  HasConditionalCsrf<paths['/api/batches/rerun-failed']['post']>,
  HasConditionalCsrf<paths['/api/reviews/auto-fix-preview']['post']>,
  HasRequiredCsrf<paths['/api/reviews/auto-fix']['post']>,
  HasRequiredCsrf<paths['/api/reviews/{id}/auto-fix']['post']>,
  HasRequiredCsrf<paths['/api/operations/unblock-jobs']['post']>,
  HasRequiredCsrf<paths['/api/operations/provider-cooldowns/clear']['post']>,
  HasRequiredCsrf<paths['/api/operations/release-scheduled-retries']['post']>,
  HasJsonBodyRejections<paths['/api/webhooks/paperless/document-consumed']['post']>,
  HasJsonBodyRejections<paths['/api/batches/rerun']['post']>,
  HasJsonBodyRejections<paths['/api/reviews/auto-fix-preview']['post']>,
  HasJsonBodyRejections<paths['/api/reviews/auto-fix']['post']>,
  HasJsonBodyRejections<paths['/api/operations/unblock-jobs']['post']>,
  HasJsonBodyRejections<paths['/api/operations/provider-cooldowns/clear']['post']>,
  // #399: extractor rejections are rewritten to the JSON error shape.
  'text/plain' extends keyof components['responses']['BadRequest']['content']
    ? false
    : true,
  'application/json' extends keyof components['responses']['UnprocessableEntity']['content']
    ? true
    : false,
  // #393: review decisions are session-only.
  HasRequiredCsrf<paths['/api/reviews/{id}/approve']['post']>,
  HasRequiredCsrf<paths['/api/reviews/{id}/reject']['post']>,
  HasRequiredCsrf<paths['/api/reviews/{id}/edit']['post']>,
  // #447: inventory facets, export and per-user saved views.
  HasOperation<'/api/inventory/facets', 'get'>,
  HasOperation<'/api/inventory/export', 'get'>,
  HasOperation<'/api/inventory/views', 'get'>,
  HasRequiredCsrf<paths['/api/inventory/views']['post']>,
  HasRequiredCsrf<paths['/api/inventory/views/{id}']['put']>,
  HasRequiredCsrf<paths['/api/inventory/views/{id}']['delete']>,
  HasJsonBodyRejections<paths['/api/inventory/views']['post']>,
  HasJsonBodyRejections<paths['/api/inventory/views/{id}']['put']>,
  // #448: audit detail for the before/after diff view.
  HasOperation<'/api/audit/{id}', 'get'>,
]>
