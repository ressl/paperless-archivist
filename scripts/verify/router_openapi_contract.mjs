import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFile } from 'node:fs/promises';

const requireFromFrontend = createRequire(
  new URL('../../frontend/package.json', import.meta.url)
);
const { parse } = requireFromFrontend('yaml');

const repositoryRoot = new URL('../../', import.meta.url);
const routerSource = await readFile(
  new URL('crates/archivist-api/src/main.rs', repositoryRoot),
  'utf8'
);
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
          pairs.add(`${method.toUpperCase()} ${joinPath(prefix, path)}`);
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

console.log(
  `Axum/OpenAPI route contract valid: ${documentedPairs.size} documented, ${internalPairs.size} internal`
);
