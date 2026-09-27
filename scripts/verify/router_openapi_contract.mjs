import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readdir, readFile } from 'node:fs/promises';

const requireFromFrontend = createRequire(
  new URL('../../frontend/package.json', import.meta.url)
);
const { parse } = requireFromFrontend('yaml');

const repositoryRoot = new URL('../../', import.meta.url);
// The runtime router is assembled only in `fn router` in main.rs; handlers
// live in `src/routes/*.rs` and are referenced from there by name. #438
const apiSourceRoot = new URL('crates/archivist-api/src/', repositoryRoot);
const routerSource = await readFile(new URL('main.rs', apiSourceRoot), 'utf8');

async function rustSourceFiles(directory, prefix = '') {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = `${prefix}${entry.name}`;
    if (entry.isDirectory()) {
      files.push(...(await rustSourceFiles(new URL(`${entry.name}/`, directory), `${relative}/`)));
    } else if (entry.name.endsWith('.rs')) {
      files.push(relative);
    }
  }
  return files;
}

// Guard the single-file assumption above: a Router built in any other
// non-test module would carry routes this verifier cannot see.
for (const relative of await rustSourceFiles(apiSourceRoot)) {
  if (relative === 'main.rs' || relative === 'test_support.rs' || relative.startsWith('tests/')) {
    continue;
  }
  const source = await readFile(new URL(relative, apiSourceRoot), 'utf8');
  assert.ok(
    !/\bRouter::new\(\)|\bfn\s+router\s*\(/.test(source),
    `crates/archivist-api/src/${relative} builds an Axum Router; routes must be declared in fn router (main.rs)`
  );
}
const openapi = parse(
  await readFile(new URL('openapi/openapi.yaml', repositoryRoot), 'utf8')
);

const httpMethods = new Set([
  'connect',
  'delete',
  'get',
  'head',
  'options',
  'patch',
  'post',
  'put',
  'trace'
]);

function scanRust(source, start, visitor) {
  let parentheses = 0;
  let brackets = 0;
  let braces = 0;
  let string = false;
  let escaped = false;
  let lineComment = false;
  let blockCommentDepth = 0;

  for (let index = start; index < source.length; index += 1) {
    const current = source[index];
    const next = source[index + 1];

    if (lineComment) {
      if (current === '\n') lineComment = false;
      continue;
    }
    if (blockCommentDepth > 0) {
      if (current === '/' && next === '*') {
        blockCommentDepth += 1;
        index += 1;
      } else if (current === '*' && next === '/') {
        blockCommentDepth -= 1;
        index += 1;
      }
      continue;
    }
    if (string) {
      if (escaped) {
        escaped = false;
      } else if (current === '\\') {
        escaped = true;
      } else if (current === '"') {
        string = false;
      }
      continue;
    }
    if (current === '/' && next === '/') {
      lineComment = true;
      index += 1;
      continue;
    }
    if (current === '/' && next === '*') {
      blockCommentDepth = 1;
      index += 1;
      continue;
    }
    if (current === '"') {
      string = true;
      continue;
    }

    if (current === '(') parentheses += 1;
    if (current === ')') parentheses -= 1;
    if (current === '[') brackets += 1;
    if (current === ']') brackets -= 1;
    if (current === '{') braces += 1;
    if (current === '}') braces -= 1;

    const result = visitor({
      index,
      current,
      parentheses,
      brackets,
      braces
    });
    if (result !== undefined) return result;
  }
  return undefined;
}

function routerFunctionBody() {
  const signature = 'fn router(state: AppState) -> Router {';
  const start = routerSource.indexOf(signature);
  assert.notEqual(start, -1, 'fn router(state: AppState) -> Router not found');
  const opening = start + signature.lastIndexOf('{');
  const closing = scanRust(routerSource, opening, (state) => {
    if (state.current === '}' && state.braces === 0) return state.index;
    return undefined;
  });
  assert.notEqual(closing, undefined, 'unterminated router function');
  return routerSource.slice(opening + 1, closing);
}

const routerFunction = routerFunctionBody();

function routerDeclarations() {
  const declarations = new Map();
  for (const match of routerFunction.matchAll(
    /\blet\s+([A-Za-z_]\w*)\s*=\s*Router::new\(\)/g
  )) {
    const name = match[1];
    assert.ok(!declarations.has(name), `duplicate local Router::new() declaration: ${name}`);
    declarations.set(name, match.index + match[0].indexOf('Router::new()'));
  }
  const names = [...declarations.keys()];
  assert.ok(names.includes('app'), 'top-level app = Router::new() declaration not found');
  return declarations;
}

function routerInitializer(name, expressionStart) {
  const end = scanRust(routerFunction, expressionStart, (state) => {
    if (
      state.current === ';' &&
      state.parentheses === 0 &&
      state.brackets === 0 &&
      state.braces === 0
    ) {
      return state.index;
    }
    return undefined;
  });
  assert.notEqual(end, undefined, `unterminated Axum router declaration: ${name}`);
  return routerFunction.slice(expressionStart, end);
}

function matchingParenthesis(source, opening) {
  const closing = scanRust(source, opening, (state) => {
    if (state.current === ')' && state.parentheses === 0) return state.index;
    return undefined;
  });
  assert.notEqual(closing, undefined, 'unterminated .route(...) call');
  return closing;
}

function callArguments(source, callName) {
  const calls = [];
  const marker = `.${callName}`;
  let searchFrom = 0;
  while (searchFrom < source.length) {
    const markerIndex = source.indexOf(marker, searchFrom);
    if (markerIndex === -1) break;
    let opening = markerIndex + marker.length;
    while (/\s/.test(source[opening])) opening += 1;
    if (source[opening] !== '(') {
      searchFrom = opening;
      continue;
    }
    const closing = matchingParenthesis(source, opening);
    calls.push(source.slice(opening + 1, closing));
    searchFrom = closing + 1;
  }
  return calls;
}

function parseStringArgument(argumentsSource, context) {
  const match = argumentsSource.match(/^\s*("(?:\\.|[^"\\])*")\s*,/);
  assert.ok(match, `${context} must start with a string path`);
  return { path: JSON.parse(match[1]), rest: argumentsSource.slice(match[0].length) };
}

function joinPath(prefix, path) {
  assert.ok(path.startsWith('/'), `Axum route/nest path must start with /: ${path}`);
  if (!prefix) return path;
  if (path === '/') return prefix;
  return `${prefix}${path}`;
}

function mountedRouterPrefixes(initializers) {
  const mounted = new Map();
  const queue = [{ name: 'app', prefix: '' }];
  const visited = new Set();

  while (queue.length > 0) {
    const current = queue.shift();
    const key = `${current.name}\u0000${current.prefix}`;
    if (visited.has(key)) continue;
    visited.add(key);
    assert.ok(initializers.has(current.name), `mounted router is not locally declared: ${current.name}`);

    const prefixes = mounted.get(current.name) ?? new Set();
    prefixes.add(current.prefix);
    mounted.set(current.name, prefixes);

    for (const argumentsSource of callArguments(initializers.get(current.name), 'nest')) {
      const { path, rest } = parseStringArgument(
        argumentsSource,
        `${current.name}.nest(...)`
      );
      const childMatch = rest.match(/^\s*([A-Za-z_]\w*)\s*$/);
      assert.ok(
        childMatch,
        `${current.name}.nest(${path}, ...) must use a locally named Router::new() value`
      );
      queue.push({ name: childMatch[1], prefix: joinPath(current.prefix, path) });
    }

    for (const argumentsSource of callArguments(initializers.get(current.name), 'merge')) {
      const childMatch = argumentsSource.match(/^\s*([A-Za-z_]\w*)\s*$/);
      assert.ok(
        childMatch,
        `${current.name}.merge(...) must use a locally named Router::new() value`
      );
      queue.push({ name: childMatch[1], prefix: current.prefix });
    }
  }

  for (const [name, source] of initializers) {
    if (callArguments(source, 'route').length > 0) {
      assert.ok(mounted.has(name), `route-bearing router is not mounted from app: ${name}`);
    }
  }
  return mounted;
}

// Local Router::new() variable each runtime route is declared on. #442
const routerOfPair = new Map();

function runtimeRoutePairs() {
  for (const unsupported of ['route_service', 'nest_service']) {
    assert.equal(
      callArguments(routerFunction, unsupported).length,
      0,
      `.${unsupported}(...) is not introspectable; document the route and extend the verifier before using it`
    );
  }
  const declarations = routerDeclarations();
  const initializers = new Map(
    [...declarations].map(([name, start]) => [name, routerInitializer(name, start)])
  );
  for (const supported of ['route', 'nest', 'merge']) {
    const discovered = callArguments(routerFunction, supported).length;
    const assigned = [...initializers.values()].reduce(
      (total, source) => total + callArguments(source, supported).length,
      0
    );
    assert.equal(
      assigned,
      discovered,
      `.${supported}(...) call exists outside a local Router::new() initializer`
    );
  }
  const mounted = mountedRouterPrefixes(initializers);
  const pairs = new Set();
  for (const [name, prefixes] of mounted) {
    const source = initializers.get(name);
    for (const argumentsSource of callArguments(source, 'route')) {
      const { path, rest: handlerSource } = parseStringArgument(
        argumentsSource,
        `${name}.route(...)`
      );
      const methods = [
        ...handlerSource.matchAll(
          /\b(connect|delete|get|head|options|patch|post|put|trace)\s*\(/g
        )
      ].map((methodMatch) => methodMatch[1]);
      assert.ok(methods.length > 0, `${name} ${path} has no recognized HTTP method`);
      for (const prefix of prefixes) {
        for (const method of methods) {
          const pair = `${method.toUpperCase()} ${joinPath(prefix, path)}`;
          pairs.add(pair);
          routerOfPair.set(pair, name);
        }
      }
    }
  }
  return pairs;
}

function internalRoutePairs(runtimePairs) {
  const pairs = new Set();
  const annotation = /^\s*\/\/\s*openapi-internal:\s*(\w+)\s+(\/\S+)\s*$/gim;
  for (const match of routerSource.matchAll(annotation)) {
    const pair = `${match[1].toUpperCase()} ${match[2]}`;
    assert.ok(runtimePairs.has(pair), `internal marker does not match a runtime route: ${pair}`);
    assert.ok(!pairs.has(pair), `duplicate internal route marker: ${pair}`);
    pairs.add(pair);
  }
  return pairs;
}

function openapiRoutePairs() {
  assert.ok(openapi?.paths, 'OpenAPI paths map must exist');
  const pairs = new Set();
  for (const [path, pathItem] of Object.entries(openapi.paths)) {
    for (const method of Object.keys(pathItem ?? {})) {
      if (httpMethods.has(method.toLowerCase())) {
        pairs.add(`${method.toUpperCase()} ${path}`);
      }
    }
  }
  return pairs;
}

function difference(left, right) {
  return [...left].filter((item) => !right.has(item)).sort();
}

const runtimePairs = runtimeRoutePairs();
const internalPairs = internalRoutePairs(runtimePairs);
const publicRuntimePairs = new Set(
  [...runtimePairs].filter((pair) => !internalPairs.has(pair))
);
const documentedPairs = openapiRoutePairs();
const undocumented = difference(publicRuntimePairs, documentedPairs);
const stale = difference(documentedPairs, publicRuntimePairs);

assert.deepEqual(
  { undocumented, stale },
  { undocumented: [], stale: [] },
  `Axum/OpenAPI path-method drift detected\n${JSON.stringify({ undocumented, stale }, null, 2)}`
);

// ----- #442: declarative route permissions ---------------------------------
//
// Every runtime route must have exactly one entry in ROUTE_POLICIES
// (crates/archivist-api/src/route_policy.rs). Routes on the `protected`
// router must require a principal; every other router must be public. The
// OpenAPI operation must mirror the declaration: `x-archivist-permission`
// names the permission and `security` the allowed auth kinds.

const policySource = await readFile(
  new URL('crates/archivist-api/src/route_policy.rs', repositoryRoot),
  'utf8'
);
const PROTECTED_ROUTER = 'protected';

function snakeCase(identifier) {
  return identifier.replace(/(?<!^)([A-Z])/g, '_$1').toLowerCase();
}

function routePolicies() {
  const tableStart = policySource.indexOf('pub(crate) const ROUTE_POLICIES');
  assert.notEqual(tableStart, -1, 'ROUTE_POLICIES table not found');
  const tableEnd = policySource.indexOf('\n];', tableStart);
  assert.notEqual(tableEnd, -1, 'unterminated ROUTE_POLICIES table');
  const table = policySource.slice(tableStart, tableEnd);
  const sessionConstants = new Set(
    [...policySource.matchAll(/const\s+(\w+):\s*AuthKinds\s*=\s*SessionOnly\(/g)].map(
      (match) => match[1]
    )
  );
  const policies = new Map();
  let entries = 0;
  for (const line of table.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('route(')) continue;
    entries += 1;
    const match = trimmed.match(
      /^route\((Get|Post|Put|Patch|Delete),\s*"([^"]+)",\s*(Public|AnyPrincipal|Require\((\w+)\)),\s*(.+)\),$/
    );
    assert.ok(match, `unparseable route policy (one entry per line): ${trimmed}`);
    const [, verb, path, access, permission, authSource] = match;
    let auth;
    if (authSource === 'SessionOrToken') auth = 'session_or_token';
    else if (authSource === 'Unauthenticated') auth = 'unauthenticated';
    else if (authSource.startsWith('SessionOnly(') || sessionConstants.has(authSource))
      auth = 'session_only';
    else assert.fail(`unknown auth kinds ${authSource} for ${verb} ${path}`);
    const declaredPermission =
      access === 'Public' ? 'public' : access === 'AnyPrincipal' ? 'authenticated' : snakeCase(permission);
    const pair = `${verb.toUpperCase()} ${path}`;
    assert.ok(!policies.has(pair), `duplicate route policy: ${pair}`);
    policies.set(pair, { permission: declaredPermission, auth });
  }
  assert.ok(entries > 0, 'ROUTE_POLICIES is empty');
  return policies;
}

const policies = routePolicies();
const undeclared = difference(runtimePairs, new Set(policies.keys()));
const stalePolicies = difference(new Set(policies.keys()), runtimePairs);
assert.deepEqual(
  { undeclared, stalePolicies },
  { undeclared: [], stalePolicies: [] },
  `Axum routes and ROUTE_POLICIES drift (every route needs a permission declaration)\n${JSON.stringify(
    { undeclared, stalePolicies },
    null,
    2
  )}`
);

const globalSecurity = openapi.security ?? [];

function schemeNames(security) {
  return security.flatMap((requirement) => Object.keys(requirement ?? {})).sort();
}

const policyProblems = [];
for (const [pair, policy] of policies) {
  const onProtectedRouter = routerOfPair.get(pair) === PROTECTED_ROUTER;
  if (onProtectedRouter === (policy.auth === 'unauthenticated')) {
    policyProblems.push(
      `${pair}: declared ${policy.auth} but mounted on router ${routerOfPair.get(pair)}`
    );
  }
  if (internalPairs.has(pair)) continue;
  const [method, path] = pair.split(' ');
  const operation = openapi.paths?.[path]?.[method.toLowerCase()];
  if (!operation) continue;
  if (operation['x-archivist-permission'] !== policy.permission) {
    policyProblems.push(
      `${pair}: x-archivist-permission ${operation['x-archivist-permission']} != ${policy.permission}`
    );
  }
  const schemes = schemeNames(operation.security ?? globalSecurity);
  const expected = {
    session_or_token: ['bearerToken', 'cookieSession'],
    session_only: ['cookieSession']
  }[policy.auth];
  if (expected && JSON.stringify(schemes) !== JSON.stringify(expected)) {
    policyProblems.push(`${pair}: OpenAPI security [${schemes}] != [${expected}] for ${policy.auth}`);
  }
  if (
    policy.auth === 'unauthenticated' &&
    (operation.security === undefined ||
      schemes.includes('cookieSession') ||
      schemes.includes('bearerToken'))
  ) {
    policyProblems.push(`${pair}: public route must declare its own (or empty) security`);
  }
}
assert.deepEqual(policyProblems, [], `Route policy contract violated\n${policyProblems.join('\n')}`);

console.log(
  `Axum/OpenAPI route contract valid: ${documentedPairs.size} documented, ${internalPairs.size} internal, ${policies.size} route policies`
);
