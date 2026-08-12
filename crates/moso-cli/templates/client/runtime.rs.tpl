// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One request, described rather than performed.
///
/// Nothing here is percent-encoded except the path, which cannot be handed over
/// as pairs. Encode `query` the way your HTTP client already does — `reqwest`'s
/// `.query(&request.query)` is exactly right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    /// The method, upper case.
    pub method: &'static str,
    /// The absolute URL, with path parameters substituted and encoded.
    pub url: String,
    /// Query parameters, in a stable order, **not** encoded.
    pub query: Vec<(String, String)>,
    /// Request headers, in a stable order.
    pub headers: Vec<(String, String)>,
    /// The body, when the operation takes one.
    pub body: Option<ApiBody>,
}

/// A request body and the media type that describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBody {
    /// What belongs in `content-type`.
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

impl ApiBody {
    /// A body of any media type.
    pub fn new(content_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            content_type: content_type.into(),
            bytes,
        }
    }

    /// `application/json`, serialised from a value.
    ///
    /// # Errors
    /// Whatever `serde_json` fails to serialise.
    pub fn json<V: serde::Serialize>(value: &V) -> Result<Self, serde_json::Error> {
        Ok(Self::new("application/json", serde_json::to_vec(value)?))
    }

    /// `text/plain`.
    pub fn text(value: impl Into<String>) -> Self {
        Self::new("text/plain; charset=utf-8", value.into().into_bytes())
    }

    /// `application/octet-stream`.
    pub fn binary(bytes: Vec<u8>) -> Self {
        Self::new("application/octet-stream", bytes)
    }
}

/// What a transport hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    /// The status code.
    pub status: u16,
    /// The response headers.
    pub headers: Vec<(String, String)>,
    /// The body, read to the end.
    pub body: Vec<u8>,
}

impl ApiResponse {
    /// The first value of one header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// How this client reaches the network.
///
/// Implement it once, over whatever HTTP client the rest of your program
/// already uses. The generated code performs no I/O and depends on no HTTP
/// crate, so it cannot make that choice for you — or drag a second TLS stack
/// into your binary.
pub trait Transport {
    /// What this transport fails with.
    type Error;

    /// Perform one request.
    fn send(
        &self,
        request: ApiRequest,
    ) -> impl core::future::Future<Output = Result<ApiResponse, Self::Error>> + Send;
}

// ---------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------

/// One field-level failure, addressed by an RFC 6901 JSON Pointer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProblemFieldError {
    /// `/title`, `/tags/2`, `/query/limit`, `/path/id`, `/header/x-tenant`.
    #[serde(default)]
    pub pointer: String,
    /// `required`, `type`, `len`, `range`, `pattern`, `format`, `enum`, …
    #[serde(default)]
    pub code: String,
    /// Human-readable and localisable. Display it; never branch on it.
    #[serde(default)]
    pub message: String,
    /// The constraint's parameters, so you can render your own message.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The RFC 9457 document every Moso error response carries.
///
/// Branch on `kind` (`type` on the wire), which identifies the problem class
/// and is stable, and on `errors[n].code`, which comes from a closed set.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProblemBody {
    /// A URI identifying the problem *class*. `type` on the wire.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// A short human-readable summary of the class.
    #[serde(default)]
    pub title: String,
    /// The status, repeated in the body as RFC 9457 asks.
    #[serde(default)]
    pub status: u16,
    /// What went wrong with *this* request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The request path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Every field that failed, not just the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ProblemFieldError>,
    /// The correlation id echoed in `x-request-id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The W3C trace id, when a tracing context was propagated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Any other member, which RFC 9457 permits.
    #[serde(flatten)]
    pub extensions: std::collections::BTreeMap<String, serde_json::Value>,
}

impl ProblemBody {
    /// The failure reported at one JSON Pointer.
    pub fn field_error(&self, pointer: &str) -> Option<&ProblemFieldError> {
        self.errors.iter().find(|entry| entry.pointer == pointer)
    }

    /// Whether any field failed with this code.
    pub fn has_code(&self, code: &str) -> bool {
        self.errors.iter().any(|entry| entry.code == code)
    }
}

/// Why a call did not produce the documented value.
#[derive(Debug)]
pub enum ApiError<E> {
    /// The transport never got an answer.
    Transport(E),
    /// The server answered with a problem document.
    Problem {
        /// The HTTP status.
        status: u16,
        /// The parsed body.
        problem: ProblemBody,
    },
    /// The server answered with something that is not the documented shape.
    Malformed {
        /// The HTTP status.
        status: u16,
        /// The bytes, kept so you can log them.
        body: Vec<u8>,
        /// What `serde_json` made of them.
        source: serde_json::Error,
    },
    /// The request body could not be serialised.
    Encode(serde_json::Error),
}

impl<E: core::fmt::Display> core::fmt::Display for ApiError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ApiError::Transport(error) => write!(f, "the request could not be sent: {error}"),
            ApiError::Problem { status, problem } => {
                write!(f, "the server answered {status}: {}", problem.title)
            }
            ApiError::Malformed { status, source, .. } => {
                write!(f, "the {status} response did not parse: {source}")
            }
            ApiError::Encode(source) => write!(f, "the request body did not serialise: {source}"),
        }
    }
}

impl<E: core::fmt::Debug + core::fmt::Display> std::error::Error for ApiError<E> {}

// ---------------------------------------------------------------------------
// Building a request
// ---------------------------------------------------------------------------

/// How a composite query parameter is spelled on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStyle {
    /// One occurrence per element. The default.
    Form,
    /// One occurrence, comma separated.
    FormJoined,
    /// `name[key]=value`.
    DeepObject,
    /// One occurrence, space separated.
    SpaceDelimited,
    /// One occurrence, pipe separated.
    PipeDelimited,
}

/// A value as it goes into a query string or a header.
fn wire_value<V: serde::Serialize>(value: &V) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::Null) | Err(_) => None,
        Ok(serde_json::Value::String(text)) => Some(text),
        Ok(other) => Some(other.to_string()),
    }
}

/// A JSON value as a query-string scalar.
fn wire_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Append one query parameter in the style the document declared.
fn push_query<V: serde::Serialize>(
    query: &mut Vec<(String, String)>,
    name: &str,
    value: &V,
    style: QueryStyle,
) {
    let Ok(value) = serde_json::to_value(value) else {
        return;
    };
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::Array(items) => match style {
            QueryStyle::Form => {
                for item in &items {
                    query.push((name.to_owned(), wire_scalar(item)));
                }
            }
            QueryStyle::DeepObject => {
                for (index, item) in items.iter().enumerate() {
                    query.push((format!("{name}[{index}]"), wire_scalar(item)));
                }
            }
            other => {
                let separator = match other {
                    QueryStyle::SpaceDelimited => " ",
                    QueryStyle::PipeDelimited => "|",
                    _ => ",",
                };
                let joined: Vec<String> = items.iter().map(wire_scalar).collect();
                query.push((name.to_owned(), joined.join(separator)));
            }
        },
        serde_json::Value::Object(members) => {
            if style == QueryStyle::DeepObject {
                for (key, item) in &members {
                    query.push((format!("{name}[{key}]"), wire_scalar(item)));
                }
            } else {
                let joined: Vec<String> = members
                    .iter()
                    .map(|(key, item)| format!("{key},{}", wire_scalar(item)))
                    .collect();
                query.push((name.to_owned(), joined.join(",")));
            }
        }
        scalar => query.push((name.to_owned(), wire_scalar(&scalar))),
    }
}

/// Append one request header, skipping an absent value.
fn push_header<V: serde::Serialize>(headers: &mut Vec<(String, String)>, name: &str, value: &V) {
    if let Some(value) = wire_value(value) {
        headers.push((name.to_owned(), value));
    }
}

/// Percent-encode one path segment, keeping only RFC 3986's unreserved set.
fn encode_path<V: serde::Serialize>(value: &V) -> String {
    let raw = wire_value(value).unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reading a response
// ---------------------------------------------------------------------------

/// Turn a 4xx or 5xx into an [`ApiError`].
fn check<E>(response: &ApiResponse) -> Result<(), ApiError<E>> {
    if response.status < 400 {
        return Ok(());
    }
    match serde_json::from_slice::<ProblemBody>(&response.body) {
        Ok(problem) => Err(ApiError::Problem {
            status: response.status,
            problem,
        }),
        Err(source) => Err(ApiError::Malformed {
            status: response.status,
            body: response.body.clone(),
            source,
        }),
    }
}

/// Decode a documented JSON body.
fn decode_json<V: serde::de::DeserializeOwned, E>(response: ApiResponse) -> Result<V, ApiError<E>> {
    check(&response)?;
    serde_json::from_slice(&response.body).map_err(|source| ApiError::Malformed {
        status: response.status,
        body: response.body.clone(),
        source,
    })
}

/// Decode a JSON body that some documented status omits.
fn decode_optional_json<V: serde::de::DeserializeOwned, E>(
    response: ApiResponse,
) -> Result<Option<V>, ApiError<E>> {
    check(&response)?;
    if response.body.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&response.body)
        .map(Some)
        .map_err(|source| ApiError::Malformed {
            status: response.status,
            body: response.body.clone(),
            source,
        })
}

/// Accept a response with no documented body.
fn decode_nothing<E>(response: ApiResponse) -> Result<(), ApiError<E>> {
    check(&response)
}

/// Decode a `text/*` body.
fn decode_text<E>(response: ApiResponse) -> Result<String, ApiError<E>> {
    check(&response)?;
    Ok(String::from_utf8_lossy(&response.body).into_owned())
}

/// Take the bytes of a body this client does not interpret.
fn decode_bytes<E>(response: ApiResponse) -> Result<Vec<u8>, ApiError<E>> {
    check(&response)?;
    Ok(response.body)
}

/// Hand back the whole response, for an operation whose body is not a value.
fn decode_raw<E>(response: ApiResponse) -> Result<ApiResponse, ApiError<E>> {
    check(&response)?;
    Ok(response)
}
