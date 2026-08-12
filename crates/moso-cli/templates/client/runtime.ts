// ---------------------------------------------------------------------------
// The problem document
// ---------------------------------------------------------------------------

/**
 * The RFC 9457 document every Moso error response carries.
 *
 * Branch on `type`, which identifies the problem *class* and is stable, and on
 * `errors[n].code`, which comes from a closed set. Never branch on `title` or
 * `message`: both are localisable, and therefore not part of the contract.
 */
export interface ProblemBody {
  readonly type: string;
  readonly title: string;
  readonly status: number;
  readonly detail?: string;
  readonly instance?: string;
  readonly errors?: readonly ProblemFieldError[];
  readonly request_id?: string;
  readonly trace_id?: string;
  readonly [key: string]: unknown;
}

/** One field-level failure, addressed by an RFC 6901 JSON Pointer. */
export interface ProblemFieldError {
  /** `/title`, `/tags/2`, `/query/limit`, `/path/id`, `/header/x-tenant`. */
  readonly pointer: string;
  /** `required`, `type`, `len`, `range`, `pattern`, `format`, `enum`, … */
  readonly code: string;
  /** Human-readable and localisable. Display it; do not branch on it. */
  readonly message: string;
  /** The constraint's parameters, so you can render your own message. */
  readonly params?: Readonly<Record<string, unknown>>;
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/**
 * What every method resolves to. None of them reject.
 *
 * `P` is the union of the schemas the operation documents for its error
 * statuses, so `failure.problem` is typed rather than `unknown`.
 */
export type ApiResult<T, P = ProblemBody> =
  | {
      readonly ok: true;
      readonly status: number;
      readonly data: T;
      readonly response: Response;
    }
  | { readonly ok: false; readonly failure: ApiFailure<P> };

/** Why a call did not succeed. */
export type ApiFailure<P = ProblemBody> =
  | {
      /** The server answered, with a body that parsed. */
      readonly kind: "problem";
      readonly status: number;
      readonly problem: P;
      readonly response: Response;
    }
  | {
      /** The server answered with something that is not the documented JSON. */
      readonly kind: "malformed";
      readonly status: number;
      readonly text: string;
      readonly cause: unknown;
      readonly response: Response;
    }
  | {
      /** The request never got an answer: DNS, TLS, an abort, offline. */
      readonly kind: "network";
      readonly cause: unknown;
    };

/** Whether a value has the shape of a problem document. */
export function isProblemBody(value: unknown): value is ProblemBody {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const candidate = value as Record<string, unknown>;
  return typeof candidate["title"] === "string" && typeof candidate["status"] === "number";
}

/** The problem document behind a failure, when there is one. */
export function problemOf(failure: ApiFailure<unknown>): ProblemBody | undefined {
  return failure.kind === "problem" && isProblemBody(failure.problem)
    ? failure.problem
    : undefined;
}

/** Every field error a failure carries, or an empty list. */
export function fieldErrors(failure: ApiFailure<unknown>): readonly ProblemFieldError[] {
  return problemOf(failure)?.errors ?? [];
}

/** The field error at one JSON Pointer, such as `/title`. */
export function fieldErrorAt(
  failure: ApiFailure<unknown>,
  pointer: string,
): ProblemFieldError | undefined {
  return fieldErrors(failure).find((entry) => entry.pointer === pointer);
}

/** Whether any field failed with this code, such as `len` or `custom:slug`. */
export function hasFieldCode(failure: ApiFailure<unknown>, code: string): boolean {
  return fieldErrors(failure).some((entry) => entry.code === code);
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/** The shape of `fetch`, so a test can substitute one. */
export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

/** How to reach the API. */
export interface ClientOptions {
  /** Prepended to every path. Defaults to the document's first server. */
  readonly baseUrl?: string;
  /** The `fetch` to call. Defaults to the global one. */
  readonly fetch?: FetchLike;
  /** Sent with every request. A function is called once per request. */
  readonly headers?: HeadersInit | (() => HeadersInit | Promise<HeadersInit>);
  /** Passed straight to `fetch`; set `"include"` to send cookies. */
  readonly credentials?: RequestCredentials;
}

/** How a composite query parameter is spelled on the wire. */
type QueryStyle = "form" | "formJoined" | "deepObject" | "spaceDelimited" | "pipeDelimited";

/** One request, as the generated methods describe it. */
interface Call {
  readonly method: string;
  readonly template: string;
  readonly path?: Readonly<Record<string, unknown>>;
  readonly query?: Readonly<Record<string, unknown>>;
  readonly styles?: Readonly<Record<string, QueryStyle>>;
  readonly headers?: Readonly<Record<string, unknown>>;
  readonly body?: unknown;
  readonly bodyKind?: "json" | "form" | "text" | "binary" | "passthrough";
  readonly accept: "json" | "text" | "binary" | "response" | "none";
  readonly init?: RequestInit;
}

/** A scalar as it goes on the wire. */
function scalar(value: unknown): string {
  return typeof value === "string" ? value : String(value);
}

/** Append one query parameter in the style the document declared. */
function appendQuery(
  search: URLSearchParams,
  name: string,
  value: unknown,
  style: QueryStyle,
): void {
  if (value === undefined || value === null) {
    return;
  }
  if (Array.isArray(value)) {
    const items = value as readonly unknown[];
    if (style === "form") {
      for (const item of items) {
        search.append(name, scalar(item));
      }
      return;
    }
    if (style === "deepObject") {
      items.forEach((item, index) => search.append(`${name}[${index}]`, scalar(item)));
      return;
    }
    const separator = style === "spaceDelimited" ? " " : style === "pipeDelimited" ? "|" : ",";
    search.append(name, items.map((item) => scalar(item)).join(separator));
    return;
  }
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    if (style === "deepObject") {
      for (const [key, item] of entries) {
        if (item !== undefined && item !== null) {
          search.append(`${name}[${key}]`, scalar(item));
        }
      }
      return;
    }
    search.append(name, entries.map(([key, item]) => `${key},${scalar(item)}`).join(","));
    return;
  }
  search.append(name, scalar(value));
}

/** `a`, `b` in a stable order, so two identical calls produce one cache key. */
function sortedKeys(record: Readonly<Record<string, unknown>>): string[] {
  return Object.keys(record).sort((left, right) => (left < right ? -1 : left > right ? 1 : 0));
}

/** Substitute the path template and append the query string. */
function buildUrl(options: ClientOptions, spec: Call): string {
  const path = spec.template.replace(/\{([^}]+)\}/g, (_match: string, name: string) => {
    const value = spec.path === undefined ? undefined : spec.path[name];
    return encodeURIComponent(value === undefined || value === null ? "" : scalar(value));
  });
  const search = new URLSearchParams();
  if (spec.query !== undefined) {
    const query = spec.query;
    for (const key of sortedKeys(query)) {
      appendQuery(search, key, query[key], spec.styles?.[key] ?? "form");
    }
  }
  const suffix = search.toString();
  const base = options.baseUrl ?? DEFAULT_BASE_URL;
  return `${base}${path}${suffix.length > 0 ? `?${suffix}` : ""}`;
}

/** `application/x-www-form-urlencoded`, from a flat object. */
function formEncode(value: unknown): string {
  const search = new URLSearchParams();
  if (typeof value === "object" && value !== null) {
    const record = value as Record<string, unknown>;
    for (const key of sortedKeys(record)) {
      const item = record[key];
      if (item === undefined || item === null) {
        continue;
      }
      if (Array.isArray(item)) {
        for (const each of item as readonly unknown[]) {
          search.append(key, scalar(each));
        }
      } else {
        search.append(key, scalar(item));
      }
    }
  }
  return search.toString();
}

/** Perform one call and decode the answer. */
async function call<T, P>(options: ClientOptions, spec: Call): Promise<ApiResult<T, P>> {
  const headers = new Headers();
  const shared = typeof options.headers === "function" ? await options.headers() : options.headers;
  if (shared !== undefined) {
    new Headers(shared).forEach((value, key) => headers.set(key, value));
  }
  if (spec.headers !== undefined) {
    const declared = spec.headers;
    for (const key of sortedKeys(declared)) {
      const value = declared[key];
      if (value !== undefined && value !== null) {
        headers.set(key, scalar(value));
      }
    }
  }
  if (spec.init?.headers !== undefined) {
    new Headers(spec.init.headers).forEach((value, key) => headers.set(key, value));
  }

  let body: BodyInit | undefined;
  switch (spec.bodyKind) {
    case "json":
      if (!headers.has("content-type")) {
        headers.set("content-type", "application/json");
      }
      body = JSON.stringify(spec.body);
      break;
    case "form":
      if (!headers.has("content-type")) {
        headers.set("content-type", "application/x-www-form-urlencoded");
      }
      body = formEncode(spec.body);
      break;
    case "text":
      if (!headers.has("content-type")) {
        headers.set("content-type", "text/plain");
      }
      body = scalar(spec.body);
      break;
    case "binary":
      if (!headers.has("content-type")) {
        headers.set("content-type", "application/octet-stream");
      }
      body = spec.body as BodyInit;
      break;
    case "passthrough":
      // FormData writes its own content-type, boundary included, so setting
      // one here would produce a body the server cannot split.
      body = spec.body as BodyInit;
      break;
    default:
      break;
  }
  if (spec.accept === "json" && !headers.has("accept")) {
    headers.set("accept", "application/json, application/problem+json");
  }

  const init: RequestInit = {
    ...spec.init,
    method: spec.method,
    headers,
    ...(body === undefined ? {} : { body }),
    ...(options.credentials === undefined ? {} : { credentials: options.credentials }),
  };

  const send: FetchLike =
    options.fetch ?? ((input: string, request: RequestInit) => fetch(input, request));

  let response: Response;
  try {
    response = await send(buildUrl(options, spec), init);
  } catch (cause) {
    return { ok: false, failure: { kind: "network", cause } };
  }

  const status = response.status;
  if (!response.ok) {
    const text = await response.text();
    try {
      return {
        ok: false,
        failure: { kind: "problem", status, problem: JSON.parse(text) as P, response },
      };
    } catch (cause) {
      return { ok: false, failure: { kind: "malformed", status, text, cause, response } };
    }
  }

  switch (spec.accept) {
    case "none":
      return { ok: true, status, data: undefined as unknown as T, response };
    case "response":
      return { ok: true, status, data: response as unknown as T, response };
    case "text":
      return { ok: true, status, data: (await response.text()) as unknown as T, response };
    case "binary":
      return { ok: true, status, data: (await response.blob()) as unknown as T, response };
    default: {
      const text = await response.text();
      if (text.length === 0) {
        return { ok: true, status, data: undefined as unknown as T, response };
      }
      try {
        return { ok: true, status, data: JSON.parse(text) as T, response };
      } catch (cause) {
        return { ok: false, failure: { kind: "malformed", status, text, cause, response } };
      }
    }
  }
}
