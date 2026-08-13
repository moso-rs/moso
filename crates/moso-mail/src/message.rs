//! What a message *is*: addresses, attachments, the [`Email`] trait, and the
//! serialisable [`RenderedEmail`] every backend actually sends.
//!
//! # Why there are two representations
//!
//! [`Email`] is what an application writes — a struct with typed fields and a
//! template. [`RenderedEmail`] is what comes out of it: subject, HTML, text,
//! headers, attachments, all resolved. The split is what lets a message cross
//! a process boundary: `RenderedEmail` is `Serialize + DeserializeOwned`, so a
//! `moso-jobs` payload can carry one, and **neither crate has to depend on the
//! other** (dependency rule 5, `xtask/allow/dep-edges.toml`).

use std::borrow::Cow;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A mailbox: an address, and optionally the display name in front of it.
///
/// ```no_run
/// use moso_mail::Address;
///
/// let to = Address::new("ada@example.com")?.with_name("Ada Lovelace");
/// assert_eq!(to.to_header(), "Ada Lovelace <ada@example.com>");
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// The mailbox, validated on construction.
    address: String,
    /// The display name, when there is one.
    name: Option<String>,
}

impl Address {
    /// Parse an address, rejecting anything that is not a mailbox.
    ///
    /// # Errors
    ///
    /// [`Error::Address`](crate::Error::Address) when the string is not a
    /// valid address. Validation goes through
    /// [`moso_schema::Email`], so an address accepted here is an address the
    /// rest of Moso accepts.
    ///
    /// ```
    /// use moso_mail::Address;
    ///
    /// assert!(Address::new("not an address").is_err());
    /// assert!(Address::new("ada@example.com").is_ok());
    /// ```
    pub fn new(address: impl Into<String>) -> Result<Self> {
        let raw = address.into();
        let email = moso_schema::Email::new(raw.clone())
            .map_err(|error| crate::Error::address(raw, error.message().to_owned()))?;
        Ok(Self::from_email(email))
    }

    /// Build from an already-validated [`moso_schema::Email`].
    ///
    /// ```
    /// use moso_mail::Address;
    /// use moso_schema::Email;
    ///
    /// let email = Email::new("ada@example.com")?;
    /// assert_eq!(Address::from_email(email).address(), "ada@example.com");
    /// # Ok::<(), moso_schema::types::ConstraintError>(())
    /// ```
    #[must_use]
    pub fn from_email(email: moso_schema::Email) -> Self {
        Self {
            address: email.into_string(),
            name: None,
        }
    }

    /// Attach a display name.
    ///
    /// The name is quoted and escaped when the header is rendered, so a name
    /// containing `<`, `"` or a newline cannot forge a second recipient.
    ///
    /// ```
    /// use moso_mail::Address;
    ///
    /// let to = Address::new("ada@example.com")?.with_name("Ada");
    /// assert_eq!(to.name(), Some("Ada"));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The bare mailbox, without the display name.
    ///
    /// ```
    /// # use moso_mail::Address;
    /// assert_eq!(Address::new("ada@example.com")?.address(), "ada@example.com");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The display name, when one was set.
    ///
    /// ```
    /// # use moso_mail::Address;
    /// assert_eq!(Address::new("ada@example.com")?.name(), None);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The domain part, for suppression and per-domain throttling.
    ///
    /// ```
    /// # use moso_mail::Address;
    /// assert_eq!(Address::new("ada@example.com")?.domain(), "example.com");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn domain(&self) -> &str {
        match self.address.rfind('@') {
            Some(at) => &self.address[at + 1..],
            // Unreachable through `new`, which parses; kept total rather than
            // panicking on a value that arrived through `Deserialize`.
            None => "",
        }
    }

    /// The address lowercased, which is how the suppression list keys it.
    ///
    /// The local part of an address is case-sensitive by the letter of RFC
    /// 5321 and case-insensitive at every real provider. Suppression follows
    /// the providers: an operator who suppresses `Ada@example.com` means the
    /// mailbox, not one spelling of it.
    ///
    /// ```
    /// # use moso_mail::Address;
    /// assert_eq!(Address::new("Ada@Example.COM")?.normalised(), "ada@example.com");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn normalised(&self) -> String {
        self.address.to_lowercase()
    }

    /// Render as an RFC 5322 header value, escaping the display name.
    ///
    /// A name containing `"`, `\`, or a control character is quoted and
    /// escaped; a name containing a newline has the newline removed outright,
    /// because a display name is the one place a header-injection attempt
    /// reaches the wire.
    ///
    /// ```
    /// # use moso_mail::Address;
    /// let to = Address::new("ada@example.com")?.with_name("Ada Lovelace");
    /// assert_eq!(to.to_header(), "Ada Lovelace <ada@example.com>");
    ///
    /// let forged = Address::new("ada@example.com")?.with_name("x\r\nBcc: evil@example.net");
    /// assert_eq!(forged.to_header(), "\"x Bcc: evil@example.net\" <ada@example.com>");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn to_header(&self) -> String {
        let Some(name) = self.name.as_deref() else {
            return self.address.clone();
        };

        // Control characters — CR and LF above all — become spaces before
        // anything else looks at the string, and runs of whitespace collapse so
        // that a stripped CRLF does not leave a double space. Quoting alone
        // would not save us: a raw CRLF inside a quoted string still ends the
        // header.
        let mut flattened = String::with_capacity(name.len());
        for c in name.chars() {
            if c.is_control() || c.is_whitespace() {
                if !flattened.ends_with(' ') {
                    flattened.push(' ');
                }
            } else {
                flattened.push(c);
            }
        }
        let flattened = flattened.trim();

        if flattened.is_empty() {
            return self.address.clone();
        }

        // An atom-only name needs no quoting; anything with a special needs
        // both quotes and backslash escapes.
        let needs_quoting = flattened.chars().any(|c| {
            matches!(
                c,
                '"' | '\\' | '<' | '>' | '(' | ')' | ',' | ':' | ';' | '@'
            )
        });
        if !needs_quoting {
            return format!("{flattened} <{}>", self.address);
        }

        let mut quoted = String::with_capacity(flattened.len() + 2);
        quoted.push('"');
        for c in flattened.chars() {
            if matches!(c, '"' | '\\') {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        format!("{quoted} <{}>", self.address)
    }
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_header())
    }
}

impl core::str::FromStr for Address {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// How an attachment is presented to the reader.
///
/// ```
/// use moso_mail::Disposition;
///
/// assert_eq!(Disposition::default(), Disposition::Attachment);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// A file the reader downloads. The default.
    #[default]
    Attachment,
    /// Displayed in the body, referenced by `cid:` from the HTML.
    Inline,
}

/// A file travelling with the message.
///
/// ```
/// use moso_mail::Attachment;
///
/// let pdf = Attachment::new("invoice.pdf", "application/pdf", vec![0u8; 12]);
/// assert_eq!(pdf.filename(), "invoice.pdf");
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    /// The name the reader sees.
    filename: String,
    /// The declared media type.
    content_type: String,
    /// The bytes.
    #[serde(with = "crate::message::base64_bytes")]
    body: Bytes,
    /// Attached or inline.
    disposition: Disposition,
    /// The `Content-ID`, for an inline attachment referenced from the HTML.
    content_id: Option<String>,
}

impl Attachment {
    /// A downloadable attachment.
    ///
    /// ```
    /// use moso_mail::Attachment;
    ///
    /// let a = Attachment::new("report.csv", "text/csv", b"a,b\n1,2\n".to_vec());
    /// assert_eq!(a.content_type(), "text/csv");
    /// ```
    #[must_use]
    pub fn new(
        filename: impl Into<String>,
        content_type: impl Into<String>,
        body: impl Into<Bytes>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            body: body.into(),
            disposition: Disposition::Attachment,
            content_id: None,
        }
    }

    /// An inline attachment, addressable from the HTML as `cid:{content_id}`.
    ///
    /// ```
    /// use moso_mail::Attachment;
    ///
    /// let logo = Attachment::inline("logo", "logo.png", "image/png", vec![0u8; 4]);
    /// assert_eq!(logo.content_id(), Some("logo"));
    /// ```
    #[must_use]
    pub fn inline(
        content_id: impl Into<String>,
        filename: impl Into<String>,
        content_type: impl Into<String>,
        body: impl Into<Bytes>,
    ) -> Self {
        Self {
            filename: filename.into(),
            content_type: content_type.into(),
            body: body.into(),
            disposition: Disposition::Inline,
            content_id: Some(content_id.into()),
        }
    }

    /// The filename.
    ///
    /// ```
    /// # use moso_mail::Attachment;
    /// # let a = Attachment::new("a.txt", "text/plain", &b"x"[..]);
    /// assert_eq!(a.filename(), "a.txt");
    /// ```
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The declared media type.
    ///
    /// ```
    /// # use moso_mail::Attachment;
    /// # let a = Attachment::new("a.txt", "text/plain", &b"x"[..]);
    /// assert_eq!(a.content_type(), "text/plain");
    /// ```
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The bytes.
    ///
    /// ```
    /// # use moso_mail::Attachment;
    /// # let a = Attachment::new("a.txt", "text/plain", &b"x"[..]);
    /// assert_eq!(a.body().len(), 1);
    /// ```
    #[must_use]
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Attached or inline.
    ///
    /// ```
    /// # use moso_mail::{Attachment, Disposition};
    /// # let a = Attachment::new("a.txt", "text/plain", &b"x"[..]);
    /// assert_eq!(a.disposition(), Disposition::Attachment);
    /// ```
    #[must_use]
    pub fn disposition(&self) -> Disposition {
        self.disposition
    }

    /// The `Content-ID` of an inline attachment.
    ///
    /// ```
    /// # use moso_mail::Attachment;
    /// # let a = Attachment::new("a.txt", "text/plain", &b"x"[..]);
    /// assert_eq!(a.content_id(), None);
    /// ```
    #[must_use]
    pub fn content_id(&self) -> Option<&str> {
        self.content_id.as_deref()
    }
}

/// Base64 for the attachment body, so a [`RenderedEmail`] survives JSON.
pub(crate) mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialise bytes as a base64 string.
    ///
    /// # Errors
    ///
    /// Whatever the serialiser reports.
    pub fn serialize<S: Serializer>(value: &Bytes, serialiser: S) -> Result<S::Ok, S::Error> {
        serialiser.serialize_str(&STANDARD.encode(value))
    }

    /// Deserialise bytes from a base64 string.
    ///
    /// # Errors
    ///
    /// When the string is not valid base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserialiser: D) -> Result<Bytes, D::Error> {
        let encoded = <std::borrow::Cow<'de, str>>::deserialize(deserialiser)?;
        STANDARD
            .decode(encoded.as_ref())
            .map(Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

/// The provider-side identifier of a sent message.
///
/// Opaque: every provider spells it differently, and the only thing an
/// application does with it is correlate a webhook back to a send.
///
/// ```
/// use moso_mail::MessageId;
///
/// let id = MessageId::new("0100018f-2c1d");
/// assert_eq!(id.as_str(), "0100018f-2c1d");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    /// Wrap a provider identifier.
    ///
    /// ```
    /// use moso_mail::MessageId;
    ///
    /// assert_eq!(MessageId::new("abc").as_str(), "abc");
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as the provider spelled it.
    ///
    /// ```
    /// # use moso_mail::MessageId;
    /// assert_eq!(MessageId::new("m1").as_str(), "m1");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for MessageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The idempotency key for a send.
///
/// A job that retries must not send twice. The key is carried to providers
/// that support idempotency and recorded locally for the ones that do not, so
/// the second attempt is a no-op rather than a second email.
///
/// ```
/// use moso_mail::MessageKey;
///
/// let key = MessageKey::new("welcome:usr_123");
/// assert_eq!(key.as_str(), "welcome:usr_123");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageKey(String);

impl MessageKey {
    /// Wrap a key. Should be stable across retries of the same logical send.
    ///
    /// ```
    /// use moso_mail::MessageKey;
    ///
    /// let key = MessageKey::new("invoice:2026-01:acct_9");
    /// assert_eq!(key.as_str(), "invoice:2026-01:acct_9");
    /// ```
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key.
    ///
    /// ```
    /// # use moso_mail::MessageKey;
    /// assert_eq!(MessageKey::new("k").as_str(), "k");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for MessageKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A message an application defines.
///
/// Four methods are required — [`to`](Email::to), [`subject`](Email::subject),
/// [`html`](Email::html) and [`text`](Email::text) — and the other ten have
/// defaults, so a message declares only what makes it different. When even
/// four is ceremony, [`Message`] builds one from values instead.
///
/// Dyn-compatible on purpose: [`Mailer::send`](crate::Mailer::send) takes
/// `&dyn Email`, so one boxed mailer sends every message type without the
/// send path being generic (compile-time rule A2, "erase early").
///
/// # Why `text` is not optional
///
/// An HTML-only message scores badly with every spam filter and is unreadable
/// in a text client. One line writes it from the HTML —
/// `Ok(moso_mail::html_to_text(&self.html()?))`, which is exactly what
/// [`Message`] does when no text part was given — so the requirement costs
/// nothing and the mail lands.
///
/// ```no_run
/// use moso_mail::{Address, Email, Result};
///
/// /// A one-line notice with no template.
/// pub struct Ping {
///     /// Who it goes to.
///     pub to: Address,
/// }
///
/// impl Email for Ping {
///     fn to(&self) -> Vec<Address> { vec![self.to.clone()] }
///     fn subject(&self) -> Result<String> { Ok("ping".to_owned()) }
///     fn html(&self) -> Result<String> { Ok("<p>ping</p>".to_owned()) }
///     fn text(&self) -> Result<String> { Ok("ping".to_owned()) }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an email",
    label = "not an email",
    note = "an email needs `to`, `subject`, `html` and `text` — the text part is required so the \
            message is readable in a text client and is not scored as spam",
    note = "help: write `impl Email for {Self}` with those four methods — `text` can be one line: \
            `Ok(moso_mail::html_to_text(&self.html()?))`",
    note = "help: or build a message from values instead of writing a type — \
            `Message::new(to).with_subject(..).with_html(..)`"
)]
pub trait Email: Send + Sync {
    /// Primary recipients. Must not be empty.
    fn to(&self) -> Vec<Address>;

    /// The rendered subject.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) when the subject is a
    /// template that fails to render.
    fn subject(&self) -> Result<String>;

    /// The HTML part.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) on a render failure.
    fn html(&self) -> Result<String>;

    /// The plain-text part. Required — never send HTML-only.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) on a render failure.
    fn text(&self) -> Result<String>;

    /// The sender, when it differs from the configured default.
    fn from(&self) -> Option<Address> {
        None
    }

    /// Where a reply should go.
    fn reply_to(&self) -> Option<Address> {
        None
    }

    /// Carbon copies.
    fn cc(&self) -> Vec<Address> {
        Vec::new()
    }

    /// Blind carbon copies.
    fn bcc(&self) -> Vec<Address> {
        Vec::new()
    }

    /// Extra headers. `List-Unsubscribe` is added automatically for a
    /// [`marketing`](Email::marketing) message.
    fn headers(&self) -> http::HeaderMap {
        http::HeaderMap::new()
    }

    /// Files travelling with the message.
    fn attachments(&self) -> Vec<Attachment> {
        Vec::new()
    }

    /// Provider-side analytics tags, e.g. `("kind", "welcome")`.
    fn tags(&self) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
        Vec::new()
    }

    /// The idempotency key. `None` means "send every time it is asked".
    fn message_key(&self) -> Option<MessageKey> {
        None
    }

    /// Whether this is marketing rather than transactional.
    ///
    /// `true` adds `List-Unsubscribe-Post`, and makes the suppression list
    /// consult the recipient's marketing preference as well as their bounce
    /// history. A marketing message **must** also carry a `List-Unsubscribe`
    /// header of its own — [`RenderedEmail::render`] refuses one that does not,
    /// because a one-click unsubscribe is a legal requirement in several
    /// jurisdictions and a deliverability requirement everywhere.
    fn marketing(&self) -> bool {
        false
    }

    /// The Rust type this message was written as, for assertions and the
    /// preview inbox.
    ///
    /// Defaulted to [`std::any::type_name`], which is the fully qualified path
    /// — `my_app::mail::WelcomeEmail`. [`RenderedEmail::kind_name`] is the last
    /// segment of it, which is what a test writes and what the inbox shows.
    /// Override only for a message type that is generated and whose Rust name
    /// means nothing to a reader.
    fn kind(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

// ---------------------------------------------------------------------------
// A message built from values
// ---------------------------------------------------------------------------

/// An [`Email`] assembled from values instead of written as a type.
///
/// Implementing [`Email`] on your own struct is the shape to reach for when a
/// message has a template, a name a test asserts on, and fields that make it
/// readable. `Message` is for the other case: a one-off notice, a message
/// whose parts a service already computed, or a test fixture — anywhere
/// defining a type would be ceremony.
///
/// It implements [`Email`], so it goes through the same
/// [`Mailer::send`](crate::Mailer::send) and the same
/// [`RenderedEmail::render`] as every other message, and is therefore held to
/// the same rules: at least one recipient, and a `List-Unsubscribe` header on
/// anything marked [`with_marketing`](Message::with_marketing).
///
/// The builders are `with_`-prefixed because the bare names — `to`, `subject`,
/// `html` — belong to the [`Email`] methods this type implements, and two
/// meanings for `message.to()` would be worse than four extra characters.
///
/// ```
/// use moso_mail::{Address, Email, Message, RenderedEmail};
///
/// let message = Message::new(Address::new("ada@example.com")?)
///     .with_subject("Your invoice")
///     .with_html("<p>Your invoice is <a href=\"https://shop.example/i/1\">ready</a>.</p>")
///     .with_kind("InvoiceReady");
///
/// // No `text` was given, so it is derived from the HTML rather than omitted:
/// // an HTML-only message is a deliverability problem, never a shortcut. The
/// // link's target survives, because a text client cannot follow an anchor.
/// assert_eq!(
///     message.text()?,
///     "Your invoice is ready (https://shop.example/i/1).",
/// );
///
/// let rendered = RenderedEmail::render(&message)?;
/// assert_eq!(rendered.kind_name(), "InvoiceReady");
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Message {
    /// The sender, when this message overrides the configured default.
    from: Option<Address>,
    /// Primary recipients. Never empty in practice: `new` takes the first.
    to: Vec<Address>,
    /// Carbon copies.
    cc: Vec<Address>,
    /// Blind copies.
    bcc: Vec<Address>,
    /// Where a reply goes.
    reply_to: Option<Address>,
    /// The subject line.
    subject: String,
    /// The HTML part.
    html: String,
    /// The text part, derived from the HTML when it was not given.
    text: Option<String>,
    /// Extra headers.
    headers: http::HeaderMap,
    /// Files travelling with the message.
    attachments: Vec<Attachment>,
    /// Provider-side analytics tags.
    tags: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    /// The idempotency key.
    message_key: Option<MessageKey>,
    /// Whether this is bulk mail.
    marketing: bool,
    /// The label tests and the preview inbox group by.
    kind: Option<&'static str>,
}

impl Message {
    /// A message to one recipient, with an empty subject and body.
    ///
    /// ```
    /// use moso_mail::{Address, Message};
    ///
    /// let _ = Message::new(Address::new("ada@example.com")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn new(to: Address) -> Self {
        Self {
            from: None,
            to: vec![to],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: None,
            subject: String::new(),
            html: String::new(),
            text: None,
            headers: http::HeaderMap::new(),
            attachments: Vec::new(),
            tags: Vec::new(),
            message_key: None,
            marketing: false,
            kind: None,
        }
    }

    /// Add another primary recipient.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let message = Message::new(Address::new("ada@example.com")?)
    ///     .with_to(Address::new("grace@example.com")?);
    /// # let _ = message;
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_to(mut self, to: Address) -> Self {
        self.to.push(to);
        self
    }

    /// Set the subject.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, Message};
    /// let message = Message::new(Address::new("a@b.com")?).with_subject("Welcome");
    /// assert_eq!(message.subject()?, "Welcome");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = subject.into();
        self
    }

    /// Set the HTML part.
    ///
    /// Usually the output of
    /// [`render_with`](crate::render_with), which is why it takes a rendered
    /// string rather than a template name: the engine belongs to the
    /// application, not to the message.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, Message};
    /// let message = Message::new(Address::new("a@b.com")?).with_html("<p>hi</p>");
    /// assert_eq!(message.html()?, "<p>hi</p>");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_html(mut self, html: impl Into<String>) -> Self {
        self.html = html.into();
        self
    }

    /// Set the text part explicitly.
    ///
    /// Without this, [`Email::text`] is derived from the HTML with
    /// [`html_to_text`](crate::html_to_text). Set it when the plain-text copy
    /// deserves to be written rather than flattened.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, Message};
    /// let message = Message::new(Address::new("a@b.com")?)
    ///     .with_html("<p>hi</p>")
    ///     .with_text("hi — the hand-written version");
    /// assert_eq!(message.text()?, "hi — the hand-written version");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Override the configured sender for this message.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let _ = Message::new(Address::new("a@b.com")?)
    ///     .with_from(Address::new("billing@shop.example")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_from(mut self, from: Address) -> Self {
        self.from = Some(from);
        self
    }

    /// Set the `Reply-To`.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let _ = Message::new(Address::new("a@b.com")?)
    ///     .with_reply_to(Address::new("support@shop.example")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_reply_to(mut self, reply_to: Address) -> Self {
        self.reply_to = Some(reply_to);
        self
    }

    /// Add a visible carbon copy.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let _ = Message::new(Address::new("a@b.com")?).with_cc(Address::new("c@d.com")?);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_cc(mut self, cc: Address) -> Self {
        self.cc.push(cc);
        self
    }

    /// Add a blind copy. It travels in the envelope and never as a header.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let audit = Address::new("audit@shop.example")?;
    /// let _ = Message::new(Address::new("a@b.com")?).with_bcc(audit);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_bcc(mut self, bcc: Address) -> Self {
        self.bcc.push(bcc);
        self
    }

    /// Add one header.
    ///
    /// Typed rather than stringly: `http::HeaderName` and `http::HeaderValue`
    /// have already refused a name or a value that could end the header early,
    /// so this builder cannot fail and cannot inject.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// use http::{HeaderName, HeaderValue};
    ///
    /// let _ = Message::new(Address::new("a@b.com")?).with_header(
    ///     HeaderName::from_static("list-unsubscribe"),
    ///     HeaderValue::from_static("<https://shop.example/u/abc>"),
    /// );
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_header(mut self, name: http::HeaderName, value: http::HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Attach a file.
    ///
    /// ```
    /// # use moso_mail::{Address, Attachment, Message};
    /// let _ = Message::new(Address::new("a@b.com")?)
    ///     .with_attachment(Attachment::new("invoice.pdf", "application/pdf", vec![0u8; 4]));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    /// Add a provider-side analytics tag.
    ///
    /// ```
    /// # use moso_mail::{Address, Message};
    /// let _ = Message::new(Address::new("a@b.com")?).with_tag("kind", "invoice");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_tag(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<Cow<'static, str>>,
    ) -> Self {
        self.tags.push((name.into(), value.into()));
        self
    }

    /// Set the idempotency key, so a retried job does not send twice.
    ///
    /// ```
    /// # use moso_mail::{Address, Message, MessageKey};
    /// let _ = Message::new(Address::new("a@b.com")?).with_key(MessageKey::new("invoice:1"));
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_key(mut self, key: MessageKey) -> Self {
        self.message_key = Some(key);
        self
    }

    /// Mark this as bulk rather than transactional.
    ///
    /// A `true` here without a `List-Unsubscribe` header is refused by
    /// [`RenderedEmail::render`], so set both or neither.
    ///
    /// ```
    /// # use moso_mail::{Address, Message, RenderedEmail};
    /// use http::{HeaderName, HeaderValue};
    ///
    /// let bulk = Message::new(Address::new("a@b.com")?)
    ///     .with_marketing(true)
    ///     .with_header(
    ///         HeaderName::from_static("list-unsubscribe"),
    ///         HeaderValue::from_static("<https://shop.example/u/abc>"),
    ///     );
    /// assert!(RenderedEmail::render(&bulk).is_ok());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_marketing(mut self, marketing: bool) -> Self {
        self.marketing = marketing;
        self
    }

    /// Set the label tests and the preview inbox group this message by.
    ///
    /// Without it every `Message` is of kind `Message`, which is useless to an
    /// assertion. A message worth asserting on is worth naming.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, Message};
    /// let message = Message::new(Address::new("a@b.com")?).with_kind("InvoiceReady");
    /// assert_eq!(message.kind(), "InvoiceReady");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn with_kind(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self
    }
}

impl Email for Message {
    fn to(&self) -> Vec<Address> {
        self.to.clone()
    }

    fn subject(&self) -> Result<String> {
        Ok(self.subject.clone())
    }

    fn html(&self) -> Result<String> {
        Ok(self.html.clone())
    }

    fn text(&self) -> Result<String> {
        Ok(match &self.text {
            Some(text) => text.clone(),
            // The rule the derive would apply, applied here for the same
            // reason: there is no legitimate HTML-only message.
            None => crate::html_to_text(&self.html),
        })
    }

    fn from(&self) -> Option<Address> {
        self.from.clone()
    }

    fn reply_to(&self) -> Option<Address> {
        self.reply_to.clone()
    }

    fn cc(&self) -> Vec<Address> {
        self.cc.clone()
    }

    fn bcc(&self) -> Vec<Address> {
        self.bcc.clone()
    }

    fn headers(&self) -> http::HeaderMap {
        self.headers.clone()
    }

    fn attachments(&self) -> Vec<Attachment> {
        self.attachments.clone()
    }

    fn tags(&self) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
        self.tags.clone()
    }

    fn message_key(&self) -> Option<MessageKey> {
        self.message_key.clone()
    }

    fn marketing(&self) -> bool {
        self.marketing
    }

    fn kind(&self) -> &'static str {
        self.kind.unwrap_or("Message")
    }
}

/// A fully rendered message: the wire form every backend sends.
///
/// This is the seam between `moso-mail` and `moso-jobs`. It is
/// `Serialize + DeserializeOwned`, so a job payload can be a `RenderedEmail`
/// and sending from a worker needs no dependency edge in either direction.
///
/// ```no_run
/// use moso_mail::{Email, RenderedEmail};
///
/// fn to_payload(message: &dyn Email) -> moso_mail::Result<RenderedEmail> {
///     RenderedEmail::render(message)
/// }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RenderedEmail {
    /// The sender. Filled from configuration when the message left it out.
    pub from: Address,
    /// Primary recipients.
    pub to: Vec<Address>,
    /// Carbon copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<Address>,
    /// Blind carbon copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<Address>,
    /// Where a reply goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Address>,
    /// The rendered subject.
    pub subject: String,
    /// The HTML part.
    pub html: String,
    /// The plain-text part.
    pub text: String,
    /// Extra headers, as name/value pairs so the struct stays serialisable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Files travelling with the message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Provider-side analytics tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<(String, String)>,
    /// The idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<MessageKey>,
    /// Whether this is a marketing message.
    #[serde(default)]
    pub marketing: bool,
    /// The Rust type name of the [`Email`] this came from, for
    /// `app.mail().assert_sent::<WelcomeEmail>(1)` and the preview inbox.
    pub kind: String,
}

impl RenderedEmail {
    /// Render an [`Email`] into its wire form.
    ///
    /// The `from` field is left as the message's own sender; a backend fills
    /// the configured default in when it is absent, because the default lives
    /// in configuration and this function does not read configuration.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) from any of the four
    /// rendering methods, [`Error::Address`](crate::Error::Address) when
    /// the recipient list is empty, and [`Error::Config`](crate::Error::Config)
    /// when a [`marketing`](Email::marketing) message carries no
    /// `List-Unsubscribe` header.
    ///
    /// ```
    /// use moso_mail::{Address, Email, RenderedEmail, Result};
    ///
    /// /// A one-line notice.
    /// struct Ping(Address);
    ///
    /// impl Email for Ping {
    ///     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
    ///     fn subject(&self) -> Result<String> { Ok("ping".to_owned()) }
    ///     fn html(&self) -> Result<String> { Ok("<p>ping</p>".to_owned()) }
    ///     fn text(&self) -> Result<String> { Ok("ping".to_owned()) }
    /// }
    ///
    /// let rendered = RenderedEmail::render(&Ping(Address::new("ada@example.com")?))?;
    /// assert_eq!(rendered.subject, "ping");
    /// assert_eq!(rendered.kind_name(), "Ping");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn render(message: &dyn Email) -> Result<Self> {
        let to = message.to();
        if to.is_empty() {
            return Err(crate::Error::address(
                String::new(),
                "a message must have at least one recipient in `to`",
            ));
        }

        // A message with no `from` is rendered as-is and the backend fills the
        // configured default in: the default lives in configuration, and this
        // function does not read configuration.
        let from = message.from().unwrap_or_else(Self::unset_sender);

        let headers: Vec<(String, String)> = message
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                // A header whose value is not UTF-8 cannot survive the JSON
                // round trip a job payload makes, and no legal header value
                // needs to. Dropping it beats corrupting it.
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();

        let marketing = message.marketing();
        let mut headers = headers;
        if marketing {
            let has_unsubscribe = headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(LIST_UNSUBSCRIBE));
            if !has_unsubscribe {
                return Err(crate::Error::config(
                    "a `marketing` message must set a `List-Unsubscribe` header — add one \
                     pointing at a one-click unsubscribe URL, or drop `marketing` if this is \
                     transactional mail",
                ));
            }
            let has_post = headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(LIST_UNSUBSCRIBE_POST));
            if !has_post {
                headers.push((
                    LIST_UNSUBSCRIBE_POST.to_owned(),
                    LIST_UNSUBSCRIBE_ONE_CLICK.to_owned(),
                ));
            }
        }

        Ok(Self {
            from,
            to,
            cc: message.cc(),
            bcc: message.bcc(),
            reply_to: message.reply_to(),
            subject: message.subject()?,
            html: message.html()?,
            text: message.text()?,
            headers,
            attachments: message.attachments(),
            tags: message
                .tags()
                .into_iter()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
            message_key: message.message_key(),
            marketing,
            kind: message.kind().to_owned(),
        })
    }

    /// Every recipient across `to`, `cc` and `bcc`.
    ///
    /// ```
    /// # use moso_mail::{Address, RenderedEmail};
    /// # fn f(r: &RenderedEmail) -> usize { r.recipients().count() }
    /// ```
    pub fn recipients(&self) -> impl Iterator<Item = &Address> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }

    /// The last segment of [`RenderedEmail::kind`] — `WelcomeEmail`, not
    /// `my_app::mail::WelcomeEmail`.
    ///
    /// What the preview inbox shows and what `app.mail().sent_of(..)` matches.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, RenderedEmail, Result};
    /// # struct Ping(Address);
    /// # impl Email for Ping {
    /// #     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
    /// #     fn subject(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn html(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn text(&self) -> Result<String> { Ok(String::new()) }
    /// # }
    /// let rendered = RenderedEmail::render(&Ping(Address::new("a@b.com")?))?;
    /// assert_eq!(rendered.kind_name(), "Ping");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn kind_name(&self) -> &str {
        self.kind.rsplit("::").next().unwrap_or(&self.kind)
    }

    /// Read one header, case-insensitively.
    ///
    /// ```
    /// # use moso_mail::RenderedEmail;
    /// # fn f(r: &RenderedEmail) -> Option<&str> { r.header("List-Unsubscribe") }
    /// ```
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether the sender is still the placeholder [`render`](RenderedEmail::render)
    /// leaves when the message set none.
    ///
    /// A backend calls this to decide whether to substitute the configured
    /// default. The placeholder is a syntactically valid address in the
    /// `.invalid` TLD (RFC 2606), so a backend that forgets to substitute
    /// produces an obviously wrong `From` rather than a plausible one.
    ///
    /// ```
    /// # use moso_mail::{Address, Email, RenderedEmail, Result};
    /// # struct Ping(Address);
    /// # impl Email for Ping {
    /// #     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
    /// #     fn subject(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn html(&self) -> Result<String> { Ok(String::new()) }
    /// #     fn text(&self) -> Result<String> { Ok(String::new()) }
    /// # }
    /// let rendered = RenderedEmail::render(&Ping(Address::new("a@b.com")?))?;
    /// assert!(rendered.sender_is_unset());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn sender_is_unset(&self) -> bool {
        self.from.address() == UNSET_SENDER
    }

    /// The placeholder sender, in the RFC 2606 `.invalid` TLD.
    fn unset_sender() -> Address {
        Address {
            address: UNSET_SENDER.to_owned(),
            name: None,
        }
    }
}

/// The header RFC 8058 requires on bulk mail.
const LIST_UNSUBSCRIBE: &str = "List-Unsubscribe";

/// The header that makes the unsubscribe one-click.
const LIST_UNSUBSCRIBE_POST: &str = "List-Unsubscribe-Post";

/// The only value RFC 8058 defines for [`LIST_UNSUBSCRIBE_POST`].
const LIST_UNSUBSCRIBE_ONE_CLICK: &str = "List-Unsubscribe=One-Click";

/// The sender a message that set none carries until a backend substitutes the
/// configured default. `.invalid` is reserved by RFC 2606 and can never resolve.
const UNSET_SENDER: &str = "unset@sender.invalid";

#[cfg(test)]
mod tests {
    use super::*;

    /// A message with nothing but the four required methods.
    struct Ping {
        /// Who it goes to.
        to: Address,
        /// Whether it is bulk.
        marketing: bool,
        /// Extra headers.
        headers: http::HeaderMap,
    }

    impl Ping {
        fn new() -> Self {
            Self {
                to: Address::new("ada@example.com").expect("valid"),
                marketing: false,
                headers: http::HeaderMap::new(),
            }
        }
    }

    impl Email for Ping {
        fn to(&self) -> Vec<Address> {
            vec![self.to.clone()]
        }
        fn subject(&self) -> Result<String> {
            Ok("ping".to_owned())
        }
        fn html(&self) -> Result<String> {
            Ok("<p>ping</p>".to_owned())
        }
        fn text(&self) -> Result<String> {
            Ok("ping".to_owned())
        }
        fn headers(&self) -> http::HeaderMap {
            self.headers.clone()
        }
        fn marketing(&self) -> bool {
            self.marketing
        }
    }

    /// A display name is the one place a header-injection attempt reaches the
    /// wire, so the CRLF must be gone before quoting even starts.
    #[test]
    fn a_display_name_cannot_forge_a_second_recipient() {
        let address = Address::new("ada@example.com")
            .expect("valid")
            .with_name("Ada\r\nBcc: evil@example.net");
        let header = address.to_header();
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
        assert_eq!(header, "\"Ada Bcc: evil@example.net\" <ada@example.com>");
    }

    /// A name needing no quoting is not quoted: a quoted display name renders
    /// with visible quotes in several clients.
    #[test]
    fn a_plain_display_name_is_left_unquoted() {
        let address = Address::new("ada@example.com")
            .expect("valid")
            .with_name("Ada Lovelace");
        assert_eq!(address.to_header(), "Ada Lovelace <ada@example.com>");
    }

    /// A name whose characters are all control characters leaves nothing to
    /// display, and an empty `"" <a@b>` is worse than a bare address.
    #[test]
    fn a_name_that_flattens_to_nothing_is_dropped() {
        let address = Address::new("ada@example.com")
            .expect("valid")
            .with_name("\r\n\t");
        assert_eq!(address.to_header(), "ada@example.com");
    }

    /// The rest of Moso validates addresses through `moso_schema::Email`, and
    /// so does this crate — one definition of "valid".
    #[test]
    fn an_address_is_validated_the_way_the_rest_of_moso_validates_one() {
        assert!(Address::new("not an address").is_err());
        assert!(Address::new("ada@localhost").is_err());
        assert!(Address::new("ada@example.com").is_ok());
    }

    /// Suppression keys on the mailbox, not on one spelling of it.
    #[test]
    fn normalisation_lowercases_the_whole_address() {
        let address = Address::new("Ada@Example.COM").expect("valid");
        assert_eq!(address.normalised(), "ada@example.com");
        // `moso_schema::Email` already lowercases the domain on parse, so the
        // only thing `normalised` still has to do is the local part.
        assert_eq!(address.domain(), "example.com");
        assert_eq!(address.address(), "Ada@example.com");
    }

    /// Every method on the trait is called exactly once and the result is a
    /// complete wire form.
    #[test]
    fn rendering_assembles_the_whole_message() {
        let rendered = RenderedEmail::render(&Ping::new()).expect("renders");
        assert_eq!(rendered.to.len(), 1);
        assert_eq!(rendered.subject, "ping");
        assert_eq!(rendered.html, "<p>ping</p>");
        assert_eq!(rendered.text, "ping");
        assert!(!rendered.marketing);
        assert!(rendered.sender_is_unset());
        assert_eq!(rendered.kind_name(), "Ping");
        assert_eq!(rendered.recipients().count(), 1);
    }

    /// A message with no recipient is a bug that must not reach a provider.
    #[test]
    fn a_message_with_no_recipient_is_refused() {
        struct Nobody;
        impl Email for Nobody {
            fn to(&self) -> Vec<Address> {
                Vec::new()
            }
            fn subject(&self) -> Result<String> {
                Ok(String::new())
            }
            fn html(&self) -> Result<String> {
                Ok(String::new())
            }
            fn text(&self) -> Result<String> {
                Ok(String::new())
            }
        }
        let error = RenderedEmail::render(&Nobody).expect_err("no recipients");
        assert!(matches!(error, crate::Error::Address { .. }));
    }

    /// Bulk mail without a one-click unsubscribe is illegal in several
    /// jurisdictions, so it fails at render time rather than at the provider.
    #[test]
    fn marketing_mail_without_an_unsubscribe_header_is_refused() {
        let mut message = Ping::new();
        message.marketing = true;
        let error = RenderedEmail::render(&message).expect_err("no List-Unsubscribe");
        assert!(matches!(error, crate::Error::Config(_)));
    }

    /// With the URL present, RFC 8058's `List-Unsubscribe-Post` is added for
    /// the application: forgetting it is what turns a one-click unsubscribe
    /// into a two-click one.
    #[test]
    fn marketing_mail_gains_the_one_click_post_header() {
        let mut message = Ping::new();
        message.marketing = true;
        message.headers.insert(
            "list-unsubscribe",
            http::HeaderValue::from_static("<https://shop.example/u/abc>"),
        );

        let rendered = RenderedEmail::render(&message).expect("renders");
        assert_eq!(
            rendered.header("List-Unsubscribe-Post"),
            Some("List-Unsubscribe=One-Click"),
        );
    }

    /// An application that set the header itself keeps its own value.
    #[test]
    fn an_explicit_post_header_is_not_overwritten() {
        let mut message = Ping::new();
        message.marketing = true;
        message.headers.insert(
            "list-unsubscribe",
            http::HeaderValue::from_static("<mailto:u@shop.example>"),
        );
        message.headers.insert(
            "list-unsubscribe-post",
            http::HeaderValue::from_static("List-Unsubscribe=One-Click"),
        );

        let rendered = RenderedEmail::render(&message).expect("renders");
        let count = rendered
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(LIST_UNSUBSCRIBE_POST))
            .count();
        assert_eq!(count, 1);
    }

    /// The whole point of `RenderedEmail`: it crosses a process boundary as a
    /// job payload, attachments and all.
    #[test]
    fn a_rendered_message_survives_a_json_round_trip() {
        struct WithFile(Address);
        impl Email for WithFile {
            fn to(&self) -> Vec<Address> {
                vec![self.0.clone()]
            }
            fn subject(&self) -> Result<String> {
                Ok("invoice".to_owned())
            }
            fn html(&self) -> Result<String> {
                Ok("<p>attached</p>".to_owned())
            }
            fn text(&self) -> Result<String> {
                Ok("attached".to_owned())
            }
            fn attachments(&self) -> Vec<Attachment> {
                vec![Attachment::new(
                    "invoice.pdf",
                    "application/pdf",
                    vec![0u8, 1, 2, 0xff],
                )]
            }
            fn message_key(&self) -> Option<MessageKey> {
                Some(MessageKey::new("invoice:1"))
            }
        }

        let original =
            RenderedEmail::render(&WithFile(Address::new("ada@example.com").expect("valid")))
                .expect("renders");

        let json = serde_json::to_string(&original).expect("serialises");
        let back: RenderedEmail = serde_json::from_str(&json).expect("deserialises");

        assert_eq!(back.subject, original.subject);
        assert_eq!(back.attachments.len(), 1);
        assert_eq!(back.attachments[0].body().as_ref(), &[0u8, 1, 2, 0xff]);
        assert_eq!(
            back.message_key.as_ref().map(MessageKey::as_str),
            Some("invoice:1")
        );
    }

    // ── the builder ──────────────────────────────────────────────────────

    /// The builder exists to remove boilerplate, not rules: what it produces
    /// goes through the same `render` as a hand-written `impl Email`.
    #[test]
    fn a_built_message_renders_like_any_other() {
        let message = Message::new(Address::new("ada@example.com").expect("valid"))
            .with_to(Address::new("grace@example.com").expect("valid"))
            .with_cc(Address::new("cc@example.com").expect("valid"))
            .with_bcc(Address::new("bcc@example.com").expect("valid"))
            .with_reply_to(Address::new("support@shop.example").expect("valid"))
            .with_from(Address::new("billing@shop.example").expect("valid"))
            .with_subject("Your invoice")
            .with_html("<p>ready</p>")
            .with_attachment(Attachment::new("i.pdf", "application/pdf", vec![1u8, 2]))
            .with_tag("kind", "invoice")
            .with_key(MessageKey::new("invoice:1"))
            .with_kind("InvoiceReady");

        let rendered = RenderedEmail::render(&message).expect("renders");
        assert_eq!(rendered.to.len(), 2);
        assert_eq!(rendered.cc.len(), 1);
        assert_eq!(rendered.bcc.len(), 1);
        assert_eq!(rendered.from.address(), "billing@shop.example");
        assert!(!rendered.sender_is_unset());
        assert_eq!(
            rendered.reply_to.as_ref().map(Address::address),
            Some("support@shop.example"),
        );
        assert_eq!(rendered.subject, "Your invoice");
        assert_eq!(rendered.attachments.len(), 1);
        assert_eq!(
            rendered.tags,
            vec![("kind".to_owned(), "invoice".to_owned())]
        );
        assert_eq!(
            rendered.message_key.as_ref().map(MessageKey::as_str),
            Some("invoice:1"),
        );
        assert_eq!(rendered.kind_name(), "InvoiceReady");
    }

    /// The one rule the builder must not let anybody skip: a text part.
    #[test]
    fn a_built_message_without_a_text_part_derives_one_from_the_html() {
        let message = Message::new(Address::new("ada@example.com").expect("valid"))
            .with_html("<p>Hello Ada</p><p>Your report is ready.</p>");

        let rendered = RenderedEmail::render(&message).expect("renders");
        assert_eq!(rendered.text, "Hello Ada\nYour report is ready.");
        assert!(!rendered.text.is_empty(), "a text part is never optional");
    }

    /// An explicitly written plain-text part beats a flattened one, and must
    /// not be quietly replaced by it.
    #[test]
    fn an_explicit_text_part_is_kept() {
        let message = Message::new(Address::new("ada@example.com").expect("valid"))
            .with_html("<p>markup</p>")
            .with_text("hand written");
        assert_eq!(message.text().expect("renders"), "hand written");
    }

    /// The builder is not a way around the bulk-mail rule: `marketing` still
    /// needs its unsubscribe header, and the header still gains the one-click
    /// directive.
    #[test]
    fn a_built_marketing_message_still_needs_its_unsubscribe_header() {
        let bare = Message::new(Address::new("ada@example.com").expect("valid"))
            .with_html("<p>news</p>")
            .with_marketing(true);
        assert!(matches!(
            RenderedEmail::render(&bare).expect_err("no List-Unsubscribe"),
            crate::Error::Config(_),
        ));

        let complete = Message::new(Address::new("ada@example.com").expect("valid"))
            .with_html("<p>news</p>")
            .with_marketing(true)
            .with_header(
                http::HeaderName::from_static("list-unsubscribe"),
                http::HeaderValue::from_static("<https://shop.example/u/abc>"),
            );
        let rendered = RenderedEmail::render(&complete).expect("renders");
        assert_eq!(
            rendered.header("List-Unsubscribe-Post"),
            Some("List-Unsubscribe=One-Click"),
        );
    }

    /// A message nobody named groups under `Message`, which is honest but
    /// useless to an assertion — hence `with_kind`.
    #[test]
    fn an_unnamed_built_message_is_of_kind_message() {
        let message = Message::new(Address::new("ada@example.com").expect("valid"));
        assert_eq!(message.kind(), "Message");
        assert_eq!(
            RenderedEmail::render(&message)
                .expect("renders")
                .kind_name(),
            "Message",
        );
    }

    /// Base64 is the only encoding that survives JSON for arbitrary bytes; a
    /// lossy one would corrupt every PDF that crossed a queue.
    #[test]
    fn attachment_bytes_are_base64_in_json() {
        let attachment = Attachment::inline("logo", "logo.png", "image/png", vec![0xffu8, 0x00]);
        let json = serde_json::to_value(&attachment).expect("serialises");
        assert_eq!(json["body"], serde_json::json!("/wA="));
        assert_eq!(json["disposition"], serde_json::json!("inline"));
        assert_eq!(json["content_id"], serde_json::json!("logo"));
    }
}
