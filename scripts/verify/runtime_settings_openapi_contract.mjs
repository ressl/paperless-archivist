import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFile } from 'node:fs/promises';

const requireFromFrontend = createRequire(
  new URL('../../frontend/package.json', import.meta.url)
);
const { parse } = requireFromFrontend('yaml');

const repositoryRoot = new URL('../../', import.meta.url);
const spec = parse(
  await readFile(new URL('openapi/openapi.yaml', repositoryRoot), 'utf8')
);
const fixture = JSON.parse(
  await readFile(new URL('openapi/fixtures/runtime-settings.json', repositoryRoot), 'utf8')
);
const inputFixture = JSON.parse(
  await readFile(new URL('openapi/fixtures/runtime-settings-input.json', repositoryRoot), 'utf8')
);
const schemas = spec?.components?.schemas;
const runtimeSchema = schemas?.RuntimeSettings;
const runtimeInputSchema = schemas?.RuntimeSettingsInput;

assert.ok(runtimeSchema, 'components.schemas.RuntimeSettings must exist');
assert.notEqual(
  runtimeSchema.additionalProperties,
  true,
  'RuntimeSettings must not be an unrestricted object'
);
assert.ok(runtimeSchema.properties, 'RuntimeSettings must declare its properties');
assert.ok(runtimeInputSchema, 'components.schemas.RuntimeSettingsInput must exist');
assert.deepEqual(
  [...(runtimeSchema.required ?? [])].sort(),
  Object.keys(fixture).sort(),
  'every RuntimeSettings response branch must be required'
);
assert.deepEqual(
  Object.keys(schemas.ProviderTuning.properties).sort(),
  Object.keys(fixture.ai.providers[0].tuning).sort(),
  'ProviderTuning fixture and OpenAPI fields must stay complete'
);
for (const [field, schema] of Object.entries(schemas.ProviderTuning.properties)) {
  assert.ok(
    Array.isArray(schema.type) && schema.type.includes('null'),
    `ProviderTuning.${field} must remain nullable for inheritance`
  );
}
assert.deepEqual(
  [...schemas.ProviderTuning.required].sort(),
  Object.keys(schemas.ProviderTuning.properties).sort(),
  'every serialized ProviderTuning field must be response-required'
);
assert.deepEqual(
  [...schemas.AiProviderSettings.required].sort(),
  Object.keys(fixture.ai.providers[0]).sort(),
  'every serialized AiProviderSettings field must be response-required'
);
assert.deepEqual(
  [...schemas.AiProviderSettingsInput.required].sort(),
  ['base_url', 'enabled', 'kind', 'name'],
  'AiProviderSettingsInput required fields must match Serde'
);
assert.equal(
  schemas.ProviderTuningInput.required,
  undefined,
  'ProviderTuningInput members must remain optional for Serde defaults'
);
assert.equal(
  runtimeInputSchema.required,
  undefined,
  'RuntimeSettingsInput branches must remain optional for Serde defaults'
);
for (const [responseName, inputName] of [
  ['RuntimeSettings', 'RuntimeSettingsInput'],
  ['PaperlessSettings', 'PaperlessSettingsInput'],
  ['PaperlessArchiveProfile', 'PaperlessArchiveProfileInput'],
  ['AiSettings', 'AiSettingsInput'],
  ['AiProviderSettings', 'AiProviderSettingsInput'],
  ['ProviderTuning', 'ProviderTuningInput'],
  ['ModelCatalogEntry', 'ModelCatalogEntryInput'],
  ['SecuritySettings', 'SecuritySettingsInput'],
  ['NotificationSettings', 'NotificationSettingsInput'],
  ['WorkflowSettings', 'WorkflowSettingsInput'],
  ['WorkflowTags', 'WorkflowTagsInput'],
  ['WorkflowRules', 'WorkflowRulesInput'],
  ['OcrSettings', 'OcrSettingsInput'],
  ['TaggingSettings', 'TaggingSettingsInput'],
  ['MetadataSettings', 'MetadataSettingsInput'],
  ['FieldSettings', 'FieldSettingsInput'],
  ['CustomFieldMapping', 'CustomFieldMappingInput'],
  ['UiSettings', 'UiSettingsInput']
]) {
  assert.deepEqual(
    Object.keys(schemas[responseName].properties).sort(),
    Object.keys(schemas[inputName].properties).sort(),
    `${responseName}/${inputName} property sets must not drift`
  );
}
for (const field of ['fallback_vision_model', 'consensus_secondary_text_model']) {
  assert.equal(
    schemas.AiSettings.properties[field].type,
    'string',
    `AiSettings.${field} must be optional but non-null in responses`
  );
  assert.ok(
    schemas.AiSettingsInput.properties[field].type.includes('null'),
    `AiSettingsInput.${field} must accept null`
  );
}
for (const field of ['label', 'usage_tier', 'context', 'modality', 'best_for']) {
  const responseType = schemas.ModelCatalogEntry.properties[field].type;
  assert.equal(responseType, 'string', `ModelCatalogEntry.${field} must be non-null`);
  assert.ok(
    schemas.ModelCatalogEntryInput.properties[field].type.includes('null'),
    `ModelCatalogEntryInput.${field} must accept null`
  );
}

function resolve(schema) {
  if (!schema?.$ref) return schema;
  const match = schema.$ref.match(/^#\/components\/schemas\/([^/]+)$/);
  assert.ok(match, `unsupported schema reference: ${schema.$ref}`);
  const resolved = schemas[match[1]];
  assert.ok(resolved, `missing schema reference: ${schema.$ref}`);
  return resolved;
}

const visitedSchemas = new Set();
function assertClosedObjectGraph(unresolvedSchema, path = 'RuntimeSettings') {
  const schema = resolve(unresolvedSchema);
  if (visitedSchemas.has(schema)) return;
  visitedSchemas.add(schema);

  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  if (types.includes('object')) {
    assert.equal(
      schema.additionalProperties,
      false,
      `${path} must explicitly reject undeclared properties`
    );
    for (const [key, property] of Object.entries(schema.properties ?? {})) {
      assertClosedObjectGraph(property, `${path}.${key}`);
    }
  }
  if (types.includes('array') && schema.items) {
    assertClosedObjectGraph(schema.items, `${path}[]`);
  }
}

function matchesType(value, type) {
  switch (type) {
    case 'null': return value === null;
    case 'object': return value !== null && typeof value === 'object' && !Array.isArray(value);
    case 'array': return Array.isArray(value);
    case 'string': return typeof value === 'string';
    case 'boolean': return typeof value === 'boolean';
    case 'integer': return Number.isInteger(value);
    case 'number': return typeof value === 'number' && Number.isFinite(value);
    default: throw new Error(`unsupported JSON Schema type: ${type}`);
  }
}

function validate(value, unresolvedSchema, path = '$') {
  const schema = resolve(unresolvedSchema);
  const types = Array.isArray(schema.type) ? schema.type : schema.type ? [schema.type] : [];
  if (types.length > 0) {
    assert.ok(
      types.some((type) => matchesType(value, type)),
      `${path} must have type ${types.join(' | ')}`
    );
  }
  if (value === null) return;
  if (schema.enum) assert.ok(schema.enum.includes(value), `${path} is not in the declared enum`);

  if (typeof value === 'number') {
    if (schema.minimum !== undefined) {
      assert.ok(value >= schema.minimum, `${path} must be >= ${schema.minimum}`);
    }
    if (schema.maximum !== undefined) {
      assert.ok(value <= schema.maximum, `${path} must be <= ${schema.maximum}`);
    }
    if (schema.format === 'int32') {
      assert.ok(value >= -2147483648 && value <= 2147483647, `${path} must fit int32`);
    }
  }
  if (typeof value === 'string' && schema.format === 'uuid') {
    assert.match(
      value,
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
      `${path} must be a UUID`
    );
  }
  if (typeof value === 'string' && schema.format === 'uri') {
    assert.doesNotThrow(() => new URL(value), `${path} must be an absolute URI`);
  }

  if (Array.isArray(value)) {
    value.forEach((item, index) => validate(item, schema.items, `${path}[${index}]`));
    return;
  }
  if (typeof value !== 'object') return;

  const properties = schema.properties ?? {};
  for (const required of schema.required ?? []) {
    assert.ok(Object.hasOwn(value, required), `${path}.${required} is required`);
  }
  for (const [key, item] of Object.entries(value)) {
    if (properties[key]) {
      validate(item, properties[key], `${path}.${key}`);
    } else if (schema.additionalProperties === false) {
      assert.fail(`${path}.${key} is not declared by the closed schema`);
    } else if (schema.additionalProperties && typeof schema.additionalProperties === 'object') {
      validate(item, schema.additionalProperties, `${path}.${key}`);
    }
  }
}

assertClosedObjectGraph(runtimeSchema);
assertClosedObjectGraph(runtimeInputSchema, 'RuntimeSettingsInput');
validate(fixture, runtimeSchema);
validate(inputFixture, runtimeInputSchema, '$input');

const omittedOptionsFixture = structuredClone(fixture);
delete omittedOptionsFixture.ai.fallback_vision_model;
delete omittedOptionsFixture.ai.consensus_secondary_text_model;
for (const field of ['label', 'usage_tier', 'context', 'modality', 'best_for']) {
  delete omittedOptionsFixture.ai.model_catalog[0][field];
}
validate(omittedOptionsFixture, runtimeSchema, '$omitted');

const unsignedBoundaryFixture = structuredClone(fixture);
unsignedBoundaryFixture.ai.providers[0].tuning.worker_concurrency = 4294967295;
unsignedBoundaryFixture.ai.providers[0].tuning.ocr_page_limit = 65535;
validate(unsignedBoundaryFixture, runtimeSchema, '$unsignedBoundary');
unsignedBoundaryFixture.ai.providers[0].tuning.worker_concurrency = 4294967296;
assert.throws(
  () => validate(unsignedBoundaryFixture, runtimeSchema),
  /worker_concurrency must be <= 4294967295/,
  'u32 tuning fields must reject values above the Rust domain'
);
unsignedBoundaryFixture.ai.providers[0].tuning.worker_concurrency = 1;
unsignedBoundaryFixture.ai.providers[0].tuning.ocr_page_limit = 65536;
assert.throws(
  () => validate(unsignedBoundaryFixture, runtimeSchema),
  /ocr_page_limit must be <= 65535/,
  'u16 tuning fields must reject values above the Rust domain'
);

const outOfRangeFixture = structuredClone(fixture);
outOfRangeFixture.paperless.timeout_seconds = 121;
assert.throws(
  () => validate(outOfRangeFixture, runtimeSchema),
  /timeout_seconds must be <= 120/,
  'contract validator must enforce numeric bounds'
);
const invalidFormatFixture = structuredClone(fixture);
invalidFormatFixture.notifications.webhook_url_secret_id = 'not-a-uuid';
assert.throws(
  () => validate(invalidFormatFixture, runtimeSchema),
  /webhook_url_secret_id must be a UUID/,
  'contract validator must enforce declared formats'
);
console.log('RuntimeSettings response/input fixtures match the closed OpenAPI schemas.');
