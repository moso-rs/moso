//! Security schemes and requirements.
//!
//! Moso's design decision here is that security is **contributed, not
//! declared**: an extractor such as `Depends<CurrentUser>` or
//! `Authorized<Delete, User>` calls
//! [`OperationBuilder::security`](crate::builder::OperationBuilder::security)
//! from its `describe`, so an endpoint's documented authentication is the
//! authentication it actually performs. The schemes themselves are declared
//! once, on the [`DocumentBuilder`](crate::builder::DocumentBuilder), because
//! the *name* of a scheme is an application-level naming decision.

use core::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an API key is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyLocation {
    /// In the query string.
    Query,
    /// In a request header.
    Header,
    /// In a cookie.
    Cookie,
}

impl ApiKeyLocation {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            ApiKeyLocation::Query => "query",
            ApiKeyLocation::Header => "header",
            ApiKeyLocation::Cookie => "cookie",
        }
    }
}

/// How clients authenticate. One entry in `components.securitySchemes`.
///
/// The `OAuth2` variant is much larger than the others because [`OAuthFlows`]
/// carries four flows with their scope maps. Boxing it would shrink the enum,
/// but security schemes are declared once at boot and there are a handful per
/// application, so the ergonomic cost of `Box` at every match site is not worth
/// paying.
///
/// ```
/// use moso_openapi::{DocumentBuilder, SecurityScheme};
///
/// let mut d = DocumentBuilder::new();
/// d.title("Shop API").version("0.1.0");
///
/// // Declare each scheme once, by name …
/// d.security_scheme("session", SecurityScheme::cookie("sid"));
/// d.security_scheme("api_key", SecurityScheme::api_key_header("x-api-key"));
/// d.security_scheme("bearer", SecurityScheme::http_bearer("JWT"));
///
/// let document = d.build().expect("a well-formed document");
/// assert_eq!(document.components.security_schemes.len(), 3);
/// ```
///
/// … then a guard or dependency names one in its `describe`, and every operation it
/// covers carries the requirement.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SecurityScheme {
    /// A key carried in a header, query parameter or cookie.
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {
        /// The header, parameter or cookie name.
        name: String,
        /// Where the key is carried.
        #[serde(rename = "in")]
        location: ApiKeyLocation,
        /// What this scheme is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `x-*` specification extensions.
        #[serde(flatten)]
        extensions: IndexMap<String, Value>,
    },
    /// RFC 7235 `Authorization` with a registered scheme, e.g. `basic` or `bearer`.
    #[serde(rename = "http", rename_all = "camelCase")]
    Http {
        /// The lowercase authentication scheme name.
        scheme: String,
        /// A hint at the bearer token's format, e.g. `JWT`. `bearer` only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_format: Option<String>,
        /// What this scheme is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `x-*` specification extensions.
        #[serde(flatten)]
        extensions: IndexMap<String, Value>,
    },
    /// Mutual TLS: the client certificate is the credential.
    #[serde(rename = "mutualTLS", rename_all = "camelCase")]
    MutualTls {
        /// What this scheme is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `x-*` specification extensions.
        #[serde(flatten)]
        extensions: IndexMap<String, Value>,
    },
    /// OAuth 2.0, described by the flows the API supports.
    #[serde(rename = "oauth2", rename_all = "camelCase")]
    OAuth2 {
        /// The supported flows.
        flows: OAuthFlows,
        /// What this scheme is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `x-*` specification extensions.
        #[serde(flatten)]
        extensions: IndexMap<String, Value>,
    },
    /// OpenID Connect Discovery.
    #[serde(rename = "openIdConnect", rename_all = "camelCase")]
    OpenIdConnect {
        /// The discovery document URL.
        open_id_connect_url: String,
        /// What this scheme is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `x-*` specification extensions.
        #[serde(flatten)]
        extensions: IndexMap<String, Value>,
    },
}

impl SecurityScheme {
    /// An API key carried in a request header.
    pub fn api_key_header(name: impl Into<String>) -> Self {
        SecurityScheme::ApiKey {
            name: name.into(),
            location: ApiKeyLocation::Header,
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// An API key carried in a query parameter.
    ///
    /// Discouraged: query strings end up in access logs and `Referer` headers.
    pub fn api_key_query(name: impl Into<String>) -> Self {
        SecurityScheme::ApiKey {
            name: name.into(),
            location: ApiKeyLocation::Query,
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// A session cookie, named `name`.
    pub fn cookie(name: impl Into<String>) -> Self {
        SecurityScheme::ApiKey {
            name: name.into(),
            location: ApiKeyLocation::Cookie,
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// HTTP authentication with an arbitrary registered scheme name.
    pub fn http(scheme: impl Into<String>) -> Self {
        SecurityScheme::Http {
            scheme: scheme.into(),
            bearer_format: None,
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// HTTP Basic authentication.
    pub fn http_basic() -> Self {
        Self::http("basic")
    }

    /// HTTP Bearer authentication, with a format hint such as `"JWT"`.
    pub fn http_bearer(bearer_format: impl Into<String>) -> Self {
        SecurityScheme::Http {
            scheme: "bearer".to_owned(),
            bearer_format: Some(bearer_format.into()),
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// Mutual TLS.
    pub fn mutual_tls() -> Self {
        SecurityScheme::MutualTls {
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// OAuth 2.0 with the given flows.
    pub fn oauth2(flows: OAuthFlows) -> Self {
        SecurityScheme::OAuth2 {
            flows,
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// OpenID Connect, discovered from `url`.
    pub fn open_id_connect(url: impl Into<String>) -> Self {
        SecurityScheme::OpenIdConnect {
            open_id_connect_url: url.into(),
            description: None,
            extensions: IndexMap::new(),
        }
    }

    /// Attach a human-readable description.
    pub fn with_description(mut self, text: impl Into<String>) -> Self {
        let slot = match &mut self {
            SecurityScheme::ApiKey { description, .. }
            | SecurityScheme::Http { description, .. }
            | SecurityScheme::MutualTls { description, .. }
            | SecurityScheme::OAuth2 { description, .. }
            | SecurityScheme::OpenIdConnect { description, .. } => description,
        };
        *slot = Some(text.into());
        self
    }

    /// The `type` discriminator this scheme serialises as.
    pub const fn kind(&self) -> &'static str {
        match self {
            SecurityScheme::ApiKey { .. } => "apiKey",
            SecurityScheme::Http { .. } => "http",
            SecurityScheme::MutualTls { .. } => "mutualTLS",
            SecurityScheme::OAuth2 { .. } => "oauth2",
            SecurityScheme::OpenIdConnect { .. } => "openIdConnect",
        }
    }

    /// The scopes this scheme can grant, if it is scope-based.
    ///
    /// Used by the documentation UI to offer a scope picker and by
    /// [`Document::validate_self`](crate::document::Document::validate_self)
    /// to reject a requirement naming a scope the scheme does not define.
    pub fn known_scopes(&self) -> Vec<&str> {
        let SecurityScheme::OAuth2 { flows, .. } = self else {
            // `openIdConnect` scopes come from the discovery document, which
            // this crate does not fetch; every other scheme is scopeless.
            return Vec::new();
        };
        let mut scopes: Vec<&str> = Vec::new();
        for (_, flow) in flows.iter() {
            for scope in flow.scopes.keys() {
                if !scopes.contains(&scope.as_str()) {
                    scopes.push(scope.as_str());
                }
            }
        }
        scopes
    }

    /// Whether a requirement naming this scheme may carry scopes.
    ///
    /// The specification says the scope list "MUST be empty" for every scheme
    /// type other than `oauth2` and `openIdConnect`.
    pub const fn accepts_scopes(&self) -> bool {
        matches!(
            self,
            SecurityScheme::OAuth2 { .. } | SecurityScheme::OpenIdConnect { .. }
        )
    }
}

/// The OAuth 2.0 flows an API supports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OAuthFlows {
    /// The implicit flow. Deprecated by OAuth 2.1; modelled because it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implicit: Option<OAuthFlow>,
    /// The resource owner password credentials flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<OAuthFlow>,
    /// The client credentials flow, for machine-to-machine access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_credentials: Option<OAuthFlow>,
    /// The authorization code flow. The one to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_code: Option<OAuthFlow>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl OAuthFlows {
    /// Only the authorization code flow.
    pub fn authorization_code(flow: OAuthFlow) -> Self {
        Self {
            authorization_code: Some(flow),
            ..Self::default()
        }
    }

    /// Only the client credentials flow.
    pub fn client_credentials(flow: OAuthFlow) -> Self {
        Self {
            client_credentials: Some(flow),
            ..Self::default()
        }
    }

    /// Iterate the flows that are present.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &OAuthFlow)> {
        [
            ("implicit", self.implicit.as_ref()),
            ("password", self.password.as_ref()),
            ("clientCredentials", self.client_credentials.as_ref()),
            ("authorizationCode", self.authorization_code.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, flow)| flow.map(|f| (name, f)))
    }
}

/// One OAuth 2.0 flow's endpoints and scopes.
///
/// Which URL members are required depends on the flow: `authorizationUrl` for
/// implicit and authorization code, `tokenUrl` for everything except implicit.
/// The model keeps both optional and
/// [`Document::validate_self`](crate::document::Document::validate_self)
/// enforces the per-flow rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OAuthFlow {
    /// Where the user is sent to authorise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    /// Where a code or credentials are exchanged for a token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// Where a refresh token is exchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Scope name to description.
    pub scopes: IndexMap<String, String>,
    /// `x-*` specification extensions.
    #[serde(flatten)]
    pub extensions: IndexMap<String, Value>,
}

impl OAuthFlow {
    /// A flow with an authorization endpoint and a token endpoint.
    pub fn new(authorization_url: impl Into<String>, token_url: impl Into<String>) -> Self {
        Self {
            authorization_url: Some(authorization_url.into()),
            token_url: Some(token_url.into()),
            ..Self::default()
        }
    }

    /// A flow with only a token endpoint, as used by client credentials.
    pub fn token_only(token_url: impl Into<String>) -> Self {
        Self {
            token_url: Some(token_url.into()),
            ..Self::default()
        }
    }

    /// Declare a scope and what it grants.
    pub fn scope(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.scopes.insert(name.into(), description.into());
        self
    }

    /// Set the refresh endpoint.
    pub fn refresh_url(mut self, url: impl Into<String>) -> Self {
        self.refresh_url = Some(url.into());
        self
    }
}

/// One alternative way to satisfy an operation's authentication.
///
/// The map is a conjunction — every named scheme must be satisfied — while a
/// list of requirements is a disjunction: any one of them suffices. An **empty**
/// requirement means "no authentication", which is how a public endpoint opts
/// out of a document-level default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecurityRequirement(IndexMap<String, Vec<String>>);

impl SecurityRequirement {
    /// An empty requirement: this operation permits unauthenticated access.
    pub fn none() -> Self {
        Self(IndexMap::new())
    }

    /// Require the named scheme with no scopes.
    pub fn scheme(name: impl Into<String>) -> Self {
        let mut map = IndexMap::new();
        map.insert(name.into(), Vec::new());
        Self(map)
    }

    /// Require the named bearer scheme. An alias for [`SecurityRequirement::scheme`]
    /// that reads better at call sites such as `SecurityRequirement::bearer("jwt")`.
    pub fn bearer(name: impl Into<String>) -> Self {
        Self::scheme(name)
    }

    /// Require the named scheme with the given scopes.
    pub fn scopes(
        name: impl Into<String>,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let mut map = IndexMap::new();
        map.insert(name.into(), scopes.into_iter().map(Into::into).collect());
        Self(map)
    }

    /// Add another scheme that must *also* be satisfied.
    pub fn and(
        mut self,
        name: impl Into<String>,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.0
            .insert(name.into(), scopes.into_iter().map(Into::into).collect());
        self
    }

    /// `true` when this requirement demands nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate the required schemes and their scopes.
    pub fn schemes(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Borrow the underlying map.
    pub fn as_map(&self) -> &IndexMap<String, Vec<String>> {
        &self.0
    }
}

impl fmt::Display for SecurityRequirement {
    /// `session + oauth[read, write]`, or `none` for the empty requirement.
    ///
    /// This rendering is the identity a [`diff`](mod@crate::diff) compares
    /// requirements by, so it has to be total and stable: every scheme is
    /// printed, in map order, and scopes are printed in declaration order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (index, (name, scopes)) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" + ")?;
            }
            f.write_str(name)?;
            if !scopes.is_empty() {
                f.write_str("[")?;
                for (index, scope) in scopes.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(scope)?;
                }
                f.write_str("]")?;
            }
        }
        Ok(())
    }
}

impl FromIterator<(String, Vec<String>)> for SecurityRequirement {
    fn from_iter<I: IntoIterator<Item = (String, Vec<String>)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn oauth() -> SecurityScheme {
        SecurityScheme::oauth2(OAuthFlows {
            authorization_code: Some(
                OAuthFlow::new("https://id.example/authorize", "https://id.example/token")
                    .scope("read", "read everything")
                    .scope("write", "write everything"),
            ),
            client_credentials: Some(
                OAuthFlow::token_only("https://id.example/token").scope("read", "read everything"),
            ),
            ..OAuthFlows::default()
        })
    }

    #[test]
    fn known_scopes_unions_every_flow_without_duplicates() {
        assert_eq!(oauth().known_scopes(), ["read", "write"]);
    }

    #[test]
    fn scopeless_schemes_know_no_scopes() {
        assert!(SecurityScheme::http_bearer("JWT").known_scopes().is_empty());
        assert!(SecurityScheme::cookie("sid").known_scopes().is_empty());
        assert!(!SecurityScheme::cookie("sid").accepts_scopes());
        assert!(oauth().accepts_scopes());
        assert!(SecurityScheme::open_id_connect("https://id.example/.well-known").accepts_scopes());
    }

    #[test]
    fn schemes_round_trip_through_json_with_their_discriminator() {
        for scheme in [
            SecurityScheme::api_key_header("x-api-key"),
            SecurityScheme::api_key_query("api_key"),
            SecurityScheme::cookie("sid"),
            SecurityScheme::http_basic(),
            SecurityScheme::http_bearer("JWT").with_description("a signed token"),
            SecurityScheme::mutual_tls(),
            oauth(),
            SecurityScheme::open_id_connect("https://id.example/.well-known/openid-configuration"),
        ] {
            let text = serde_json::to_string(&scheme).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["type"], json!(scheme.kind()), "{text}");
            let back: SecurityScheme = serde_json::from_str(&text).unwrap();
            assert_eq!(scheme, back, "{text}");
        }
    }

    #[test]
    fn scheme_extensions_round_trip() {
        let text = r#"{"type":"http","scheme":"bearer","x-note":"internal"}"#;
        let scheme: SecurityScheme = serde_json::from_str(text).unwrap();
        let SecurityScheme::Http { extensions, .. } = &scheme else {
            panic!("expected an http scheme");
        };
        assert_eq!(extensions.get("x-note"), Some(&json!("internal")));
        assert_eq!(
            serde_json::from_str::<SecurityScheme>(&serde_json::to_string(&scheme).unwrap())
                .unwrap(),
            scheme
        );
    }

    #[test]
    fn requirements_render_for_diagnostics() {
        assert_eq!(SecurityRequirement::none().to_string(), "none");
        assert_eq!(
            SecurityRequirement::scheme("session").to_string(),
            "session"
        );
        assert_eq!(
            SecurityRequirement::scopes("oauth", ["read", "write"]).to_string(),
            "oauth[read, write]"
        );
        assert_eq!(
            SecurityRequirement::scheme("session")
                .and("oauth", ["read"])
                .to_string(),
            "session + oauth[read]"
        );
    }

    #[test]
    fn requirements_serialise_transparently() {
        let requirement = SecurityRequirement::scopes("oauth", ["read"]);
        assert_eq!(
            serde_json::to_string(&requirement).unwrap(),
            r#"{"oauth":["read"]}"#
        );
        let back: SecurityRequirement = serde_json::from_str(r#"{"oauth":["read"]}"#).unwrap();
        assert_eq!(back, requirement);
        assert!(!back.is_empty());
        assert!(
            serde_json::from_str::<SecurityRequirement>("{}")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn flows_iterate_in_the_documented_order() {
        let flows = OAuthFlows {
            implicit: Some(OAuthFlow::default()),
            password: Some(OAuthFlow::default()),
            client_credentials: Some(OAuthFlow::default()),
            authorization_code: Some(OAuthFlow::default()),
            ..OAuthFlows::default()
        };
        let names: Vec<_> = flows.iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "implicit",
                "password",
                "clientCredentials",
                "authorizationCode"
            ]
        );
    }
}
