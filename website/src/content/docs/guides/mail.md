---
title: Sending mail
description: Compose typed messages, render Jinja templates, choose a backend in configuration, browse a development inbox, keep a suppression list and verify provider delivery webhooks.
order: 31
status: shipped
---

`moso-mail` gives you one trait an application implements per message, one trait a backend
implements, and a fixed set of wrappers between them. You write `Welcome`, `PasswordReset` and
`Invoice` as types. You pick console, SMTP or one of five REST providers in configuration. Nothing
in a handler ever names the backend, so the same code sends to a browsable inbox in development,
into a `Vec` in a test and through SES in production.

You write a message type by implementing four `Email` trait methods by hand, or build one with
`Message` when a type would be ceremony. The `moso-test` feature ships an `app.mail()` handle and
`capture_mail`'s `assert_sent::<T>(n)` / `assert_none_sent()`; the `MemoryMailer` assertions below
are the lower-level form they wrap. Everything on this page is shipped and covered by tests.

## Turning it on

The `moso` facade does not re-export `moso-mail`. Add the crate to your own manifest.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1" }
moso-mail = { version = "0.1" }
```

`console` and `memory` are on by default because neither needs a service running. Every other
backend is a feature you opt into, and asking for one you did not compile is a boot error naming
the exact line to add.

| Feature | Default | Adds | Needs |
| --- | --- | --- | --- |
| `console` | yes | `backend::ConsoleMailer` and the `/_mail` preview inbox | nothing |
| `memory` | yes | `backend::MemoryMailer` | nothing |
| `file` | no | `backend::FileMailer`, one `.eml` per message | a writable directory |
| `mail-smtp` | no | `backend::SmtpMailer` (pulls `lettre`) | an SMTP server |
| `mail-ses` | no | `MailProvider::Ses` and `webhook::SnsVerifier` | AWS credentials |
| `mail-sendgrid` | no | `MailProvider::Sendgrid` | an API key |
| `mail-postmark` | no | `MailProvider::Postmark` | a server token |
| `mail-resend` | no | `MailProvider::Resend` | an API key |
| `mail-mailgun` | no | `MailProvider::Mailgun` | an API key and a domain |

`provider` is a private feature that the five REST backends turn on for you. It pulls `reqwest` and
`rustls`, so a build with any provider costs noticeably more compile time than one without.

## The smallest thing that sends

A message is a type. Implement `Email` and hand it to a `Mailer`.

```rust title="src/mail/welcome.rs"
use moso_mail::{Address, Email, Mailer, Result};

/// The message sent to a new account.
pub struct Welcome {
    /// Who signed up.
    pub to: Address,
    /// The link that verifies their address.
    pub verify_url: String,
}

impl Email for Welcome {
    fn to(&self) -> Vec<Address> { vec![self.to.clone()] }
    fn subject(&self) -> Result<String> { Ok("Welcome".to_owned()) }
    fn html(&self) -> Result<String> { Ok(format!("<a href={:?}>verify</a>", self.verify_url)) }
    fn text(&self) -> Result<String> { Ok(format!("verify: {}", self.verify_url)) }
}

async fn welcome(mailer: &dyn Mailer, message: &Welcome) -> Result<()> {
    mailer.send(message).await?;
    Ok(())
}
```

Four methods are required and `text` is one of them, deliberately. There is no default, because an
HTML-only message is a deliverability problem you find in a spam folder rather than in a log. When
you have no separate copy to write, generate it:

```rust
fn text(&self) -> moso_mail::Result<String> {
    Ok(moso_mail::html_to_text(&self.html()?))
}
```

In a handler the mailer arrives through [dependency injection](./dependency-injection.md) as a trait
object, so nothing in the signature knows which backend is behind it.

```rust title="src/routes/signup.rs"
use moso::prelude::*;
use moso::response::NoContent;
use moso_mail::{Address, Mailer};

use crate::mail::welcome::Welcome;

/// Create an account and greet it.
#[endpoint]
async fn signup(Inject(mailer): Inject<dyn Mailer>) -> Result<NoContent> {
    let message = Welcome {
        to: Address::new("ada@example.com")?,
        verify_url: "https://shop.example/verify/abc".to_owned(),
    };
    mailer.send(&message).await?;
    Ok(NoContent)
}
```

`Address::new` validates through `moso_schema::Email`, so an unparseable address is an error at the
point you build it rather than a 500 from a provider later. `moso_mail::Error` converts into
`moso::Error`, which is why `?` works in a handler at all.

### When a type is ceremony

A message that has a template, a name a test asserts on and fields that make it readable earns
its own type. A one-off notice does not. `Message` builds one from values and implements `Email`,
so it travels the same path as everything else:

```rust
use moso_mail::{Address, Mailer, Message, MessageKey};

async fn notify(mailer: &dyn Mailer, to: Address, url: &str) -> moso_mail::Result<()> {
    let message = Message::new(to)
        .with_subject("Your export is ready")
        .with_html(format!("<p>Your export is <a href=\"{url}\">ready</a>.</p>"))
        .with_tag("kind", "export")
        .with_key(MessageKey::new("export:42"))
        .with_kind("ExportReady");

    mailer.send(&message).await?;
    Ok(())
}
```

The builders are `with_`-prefixed because the bare names (`to`, `subject`, `html`) are the
`Email` methods this type implements. Give no `with_text` and the text part is derived from the
HTML with `html_to_text`, which is the rule a derive would apply: there is no HTML-only message.
`with_kind` is what an assertion and the preview inbox group by; without it every built message is
of kind `Message`, which is honest and useless.

`Message` is not a way around any rule. `RenderedEmail::render` still refuses an empty recipient
list, and still refuses `with_marketing(true)` without a `List-Unsubscribe` header.

## Composing a message

`Email` has fourteen methods. Four are required; the rest have defaults, so you override only what
a particular message needs.

| Method | Default | What it does |
| --- | --- | --- |
| `to() -> Vec<Address>` | required | The recipients. An empty list fails at render time. |
| `subject() -> Result<String>` | required | The subject, RFC 2047 encoded on the wire when it needs it. |
| `html() -> Result<String>` | required | The HTML part. |
| `text() -> Result<String>` | required | The plain-text part. Never optional. |
| `from() -> Option<Address>` | `None` | Overrides the configured sender for this one message. |
| `reply_to() -> Option<Address>` | `None` | A `Reply-To` header. |
| `cc() -> Vec<Address>` | empty | Visible carbon copies. |
| `bcc() -> Vec<Address>` | empty | Blind copies. Carried in the envelope, never written as a header. |
| `headers() -> http::HeaderMap` | empty | Extra headers. `Bcc`, `Message-ID`, `Date` and `MIME-Version` are filtered out. |
| `attachments() -> Vec<Attachment>` | empty | Files travelling with the message. |
| `tags() -> Vec<(Cow<str>, Cow<str>)>` | empty | Provider analytics tags, dropped by backends that have none. |
| `message_key() -> Option<MessageKey>` | `None` | An idempotency key, used by the backends that support one. |
| `marketing() -> bool` | `false` | Marks the message as bulk. See the warning below. |
| `kind() -> &'static str` | `type_name::<Self>()` | The label tests and logs group by. |

### Addresses

`Address` is a validated mailbox with an optional display name. `with_name` attaches one,
`normalised()` lowercases it (that is the suppression list's key), and `to_header()` produces the
RFC 5322 value. A display name cannot forge a recipient: `to_header` flattens every control
character before quoting, so a `\r\n` inside a name cannot end the header and start another.

```rust
use moso_mail::Address;

let to = Address::new("ada@example.com")?.with_name("Ada Lovelace");
assert_eq!(to.to_header(), "Ada Lovelace <ada@example.com>");
assert_eq!(to.domain(), "example.com");
assert!(Address::new("Ada <ada@example.com>").is_err());
```

`Address::new` takes a bare mailbox. The display-name form is what `to_header` produces, not what
`new` parses, so build it with `with_name`. `MailConfig::new` is the one place that accepts
`"Shop <hello@shop.example>"`, because that is how a configuration file writes a sender.

### Attachments

`Attachment::new` produces a downloadable file. `Attachment::inline` produces one addressable from
the HTML as `cid:{content_id}`, which is what puts a logo in a message without a remote image.

```rust
use moso_mail::{Attachment, Disposition};

let receipt = Attachment::new("receipt.pdf", "application/pdf", pdf_bytes);
let logo = Attachment::inline("logo", "logo.png", "image/png", png_bytes);
assert_eq!(logo.disposition(), Disposition::Inline);
assert_eq!(logo.content_id(), Some("logo"));
```

Moso builds its own MIME (`moso_mail::mime::to_rfc5322`), and the nesting follows what the message
actually carries: `multipart/alternative` on its own, wrapped in `multipart/related` when there are
inline parts, wrapped again in `multipart/mixed` when there are ordinary attachments. The text part
always precedes the HTML part, which is what clients expect when they pick one.

Attachment size limits are per backend and are real numbers you can read before you act, through
`Mailer::capabilities`. See the backend table below.

> [!WARNING]
> A message whose `marketing()` returns `true` and which carries no `List-Unsubscribe` header fails
> at render time with `Error::Config`. That is the point: bulk mail without an unsubscribe header is
> a deliverability incident waiting to happen. `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
> is added for you when the header is present but the post directive is not.

## Templates

The shipped engine is `Jinja`, a Jinja2-compatible renderer with two opinions.

Undefined variables are strict, so a typo fails the render instead of sending "Hello ,". And
autoescaping is decided per template from its extension: `.html`, `.htm` and `.xhtml` escape,
everything else does not. Name your text template `welcome.txt` and your HTML one `welcome.html`
and both behave correctly without a flag.

```rust
let mut engine = moso_mail::Jinja::new();
engine
    .add(Template::inline(
        "emails/welcome.html",
        "<p>Hi {{ user.name }},</p>\
         <p><a href=\"{{ verify_url }}\">verify</a></p>\
         {% if user.trial %}<p>{{ app_name }}</p>{% endif %}",
    ))
    .expect("the template parses");

assert_eq!(
    engine.variables("emails/welcome.html"),
    vec![
        "app_name".to_owned(),
        "user.name".to_owned(),
        "user.trial".to_owned(),
        "verify_url".to_owned(),
    ],
);

let error = engine
    .render(
        "emails/welcome.html",
        &serde_json::json!({ "user": { "name": "Ada", "trial": false } }),
    )
    .expect_err("`verify_url` is undefined");
assert!(matches!(error, moso_mail::Error::Template { .. }));
```

`Template::inline` embeds the source in the binary at compile time. `Template::from_path` reads it
from disk, which is convenient for iteration and means a broken template is discovered at
`engine.add(..)` rather than at the first send. Register every template once at boot for that
reason.

`TemplateEngine::variables` returns the dotted paths a template references, sorted. Since you write
the `Email` methods by hand, this is what a test compares against the keys your context builder puts
in, so a template variable your context never sets is caught before the first send.

`render_with(engine, name, &context)` takes any `Serialize` value instead of a `serde_json::Value`,
which is usually what you want:

```rust
use moso_mail::{TemplateEngine, render_with};

fn html(engine: &dyn TemplateEngine, ctx: &WelcomeContext) -> moso_mail::Result<String> {
    render_with(engine, "emails/welcome.html", ctx)
}
```

## Backends

You choose one in configuration and never in code.

| Backend | `MailBackendKind` | Feature | Where the message goes |
| --- | --- | --- | --- |
| Console | `Console` | `console` | stdout, plus the `/_mail` inbox |
| File | `File` | `file` | one `.eml` per message in a directory |
| Memory | `Memory` | `memory` | a `Vec` you can assert on |
| SMTP | `Smtp` | `mail-smtp` | a pooled SMTP connection |
| SES | `Ses` | `mail-ses` | `POST /v2/email/outbound-emails`, signed with SigV4 |
| SendGrid | `Sendgrid` | `mail-sendgrid` | `POST /v3/mail/send` |
| Postmark | `Postmark` | `mail-postmark` | `POST /email` |
| Resend | `Resend` | `mail-resend` | `POST /emails` |
| Mailgun | `Mailgun` | `mail-mailgun` | `POST /v3/{domain}/messages.mime` |

Each backend reports what it can actually do through `MailCapabilities`, so a caller can branch on
data rather than discover a limit from a 400.

| Backend | Batching | Max batch | Tracking | Max attachment | Max recipients | Tags | Webhooks | Scheduling | Idempotency |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| console, file, memory | yes | unbounded | no | 10 MiB | 50 | yes | no | no | yes |
| smtp | no | 0 | no | 25 MiB | 100 | no | no | no | no |
| SES | no | 0 | yes | 10 MiB | 50 | yes | yes | no | no |
| SendGrid | yes | 1000 | yes | 30 MiB | 1000 | yes | yes | yes | no |
| Postmark | yes | 500 | yes | 10 MiB | 50 | yes | yes | no | no |
| Resend | yes | 100 | yes | 40 MiB | 50 | yes | yes | yes | yes |
| Mailgun | yes | 1000 | yes | 25 MiB | 1000 | yes | yes | yes | no |

A backend that cannot batch returns `Error::Unsupported` from `send_batch` rather than looping over
the messages, so a caller never believes it made one request when it made a thousand.

Every backend in that table enforces the send deadline described under
[configuration](#the-send-deadline), and each takes a `timeout(Duration)` builder of its own for
the case where you construct it directly rather than through `MailConfig`.

### SMTP

`SmtpMailer::from_url` parses a DSN, which is what a configuration file usually carries.

```text
smtp://user:pass@mail.example:587?security=starttls
smtps://user:pass@mail.example:465
smtp://localhost:1025?security=none
```

The scheme picks the default port (587 for `smtp`, 465 for `smtps`), `%XX` escapes in the
credentials are decoded, and `?security=` takes `starttls`, `tls`, `implicit` or `none`.

> [!IMPORTANT]
> `security=none` is refused unless the host is `localhost` or a loopback address. Plaintext SMTP
> across a network is a credential leak, and the only legitimate use of it is a local mail catcher.

`SmtpMailer` pools connections (`pool_size`) and implements `probe`, so it can back a readiness
check.

### REST providers

`ProviderMailer::new(provider, api_key)` plus the builder methods your provider needs (`region` for
SES, `domain` for Mailgun, `base_url` to point at a gateway or a test double). Response handling is
uniform: 429 and 5xx become `Error::Unavailable`, which is retryable, and every other non-success
becomes `Error::Rejected`, which is not. The message id is read from the `x-message-id` header
first and then from the `id`, `MessageID`, `message_id` or `MessageId` JSON keys.

## Configuration and boot

`MailConfig` is a plain struct with a builder. Neither this crate nor `moso-storage` reads an
environment variable: mapping your configuration onto these fields is your application's code, done
once at boot. See [configuration](./configuration.md) for how the typed config gets there.

| Field | Default | Effect |
| --- | --- | --- |
| `from` | required | The default sender, accepting `"Shop <hello@shop.example>"` |
| `backend` | `Console` | Which backend to build |
| `url` | `None` | The DSN or API key, as a `SecretString` |
| `region` | `None` | SES region, defaulting to `us-east-1` |
| `domain` | `None` | Mailgun sending domain, required for Mailgun |
| `directory` | `None` | Where `File` writes, required for `File` |
| `timeout` | 30s | The per-send deadline, enforced by every backend |
| `redirect_to` | `None` | Send everything to one address instead |
| `preview` | local backends only | Whether to expose `/_mail` |
| `suppression` | `true` | Whether to wrap the backend in `Suppressing` |

The error text of `validate` names the configuration keys an application is expected to read:
`MAIL_BACKEND`, `MAIL_URL`, `MAIL_DIRECTORY`, `MAIL_DOMAIN`, `MAIL_TIMEOUT`, `MAIL_PREVIEW`,
`MAIL_REDIRECT_TO` and `MAIL_SUPPRESSION`.

```rust title="src/boot.rs"
use std::sync::Arc;

use moso_mail::{MailBackendKind, MailConfig, Mailer, MemorySuppressionList, SuppressionList};

fn mailer() -> Result<Arc<dyn Mailer>, Box<dyn std::error::Error>> {
    let suppression: Arc<dyn SuppressionList> = Arc::new(MemorySuppressionList::new());
    let config = MailConfig::new("Shop <hello@shop.example>", MailBackendKind::Console)?;

    config.validate()?;
    for warning in config.warnings(false) {
        tracing::warn!("{warning}");
    }

    Ok(config.build(Some(suppression))?)
}
```

`validate` refuses contradictions outright: a remote backend with no `url`, `File` with no
`directory`, `Mailgun` with no `domain`, a zero timeout. `warnings(production)` returns advisory
lines that do not stop the boot: sending through a local backend in production, serving `/_mail` in
production, redirecting every message in production, suppression turned off.

`build` composes in a fixed order, and the order is load bearing:

1. the base backend for the chosen `MailBackendKind`,
2. `Redirecting` when `redirect_to` is set,
3. `Suppressing` outermost when you supplied a list and `suppression` is on.

Suppression outermost means it sees the message's real recipients. Were it inside the redirect it
would only ever check the staging inbox, and a suppression bug would first show up in production.

Register the result and handlers can inject it:

```rust
let app = App::new(config)
    .provide_dyn::<dyn Mailer>(mailer()?)
    .mount(routes());
```

### The send deadline

`MailConfig::timeout` is a deadline, not advice. `build` hands it to whichever backend it
constructs, and every backend (console, file, memory, SMTP and all five providers) wraps the
whole of its send in it. A provider that accepts the connection and then stops talking costs you
that deadline and nothing more.

```rust
use std::time::Duration;

let config = MailConfig::new("Shop <hello@shop.example>", MailBackendKind::Smtp)?
    .url("smtp://user:pass@mail.example:587")
    .timeout(Duration::from_secs(10));
```

Overrunning is `Error::Timeout { backend, after }`, which is a distinct variant rather than a
generic transport failure: it names the backend and the deadline that elapsed, it is `retryable()`,
and it becomes a **504** over HTTP. For SMTP the deadline covers the entire conversation (connect,
TLS, `AUTH`, `DATA` and the final `250`), because a per-write timeout never fires against a server
that answers one byte at a time and never finishes.

A timed-out send may still have been accepted; only the answer was lost. That is exactly what
`message_key` is for, so give anything you retry an idempotency key.

Constructing a backend by hand instead of through `MailConfig` gets the same deadline: each one
takes a `timeout(Duration)` builder, defaulting to `moso_mail::deadline::DEFAULT_TIMEOUT` (30s). A
hand-written `Mailer` of your own should wrap its send in `moso_mail::deadline::within`, which is
the one place the rule lives.

## The local preview inbox

The console and memory backends implement `preview::Inbox`, and `preview::routes` turns one into a
router mounted at `/_mail`: an index, the HTML part in a sandboxed iframe, the text part, a
downloadable `.eml` and a clear button.

```rust title="src/boot.rs"
use std::sync::Arc;

use moso_mail::backend::ConsoleMailer;
use moso_mail::preview::{Inbox, routes};

let console = Arc::new(ConsoleMailer::new().keep(100));
let mailer: Arc<dyn moso_mail::Mailer> = console.clone();
let inbox: Arc<dyn Inbox> = console;

let router = app_routes().merge(routes(inbox));
```

Mounting it needs the concrete `Arc<ConsoleMailer>`, not the `Arc<dyn Mailer>` that
`MailConfig::build` returns, because `Inbox` is implemented by the backend and not by the trait
object. That is also why a remote backend has no inbox: there is nothing local to browse.

Three details of the inbox are security decisions rather than conveniences. The HTML part is served
with `Content-Security-Policy: sandbox; default-src 'none'; style-src 'unsafe-inline'` and
`X-Content-Type-Options: nosniff`, because a preview that executes a message's scripts is a self-XSS
in your own origin. Every response carries `Cache-Control: no-store`. And the routes go through
`Router::mount_axum`, so they are absent from the [OpenAPI document](./openapi.md) by construction:
a development inbox is not part of anybody's API.

`preview` defaults to on only for a local backend, and `warnings(true)` complains if it is on in
production, because message bodies contain password-reset links.

## Sending from a background job

The recommended shape is that a request renders a message and enqueues it, and a worker sends it.
That keeps a provider round trip out of the request path and gets you retries and a dead-letter
queue from [the jobs battery](./jobs.md) for free.

`RenderedEmail` is the seam. `RenderedEmail::render(&message)` calls all fourteen `Email` methods
once, enforces the non-empty recipient list and the marketing rules, and produces a
`Serialize + DeserializeOwned` value. That value is a legal job payload.

```rust title="src/jobs/mail.rs"
use moso::jobs::prelude::*;
use moso::prelude::Inject;
use moso_mail::{Mailer, RenderedEmail};

/// Deliver one already-rendered message.
#[job(queue = "mail", retries = 5, backoff = "exponential(30s, max = 1h)")]
pub async fn deliver(args: RenderedEmail, Inject(mailer): Inject<dyn Mailer>) -> Result {
    mailer.send_rendered(&args).await?;
    Ok(())
}
```

The handler side is one call:

```rust
let rendered = RenderedEmail::render(&message)?;
deliver.enqueue(rendered).await?;
```

Neither crate depends on the other. `moso-mail` has an empty allowed-dependency list, enforced by
`xtask check-deps`, so a stateless service that sends one email does not compile a queue and a
database driver to do it.

`Mailer::send_now` exists for the opposite case: an application whose mailer is wrapped in a
queueing decorator can still force one message straight out at the call site.

## Suppression lists

A suppression list records addresses you must not send to, and `Suppressing` consults it before
every send. It is a wrapper rather than a flag, so no backend can forget the check.

```rust
use std::sync::Arc;
use moso_mail::backend::Suppressing;
use moso_mail::{MemorySuppressionList, Mailer};

let inner: Arc<dyn Mailer> = Arc::new(moso_mail::backend::MemoryMailer::new());
let list = Arc::new(MemorySuppressionList::new());
let mailer = Suppressing::new(inner, list);
```

A reason distinguishes a permanent block from a marketing-only one, which is why a password reset
still reaches somebody who left your newsletter.

| `SuppressionReason` | Permanent | Blocks transactional | `describe_reason` |
| --- | --- | --- | --- |
| `HardBounce` | yes | yes | `hard bounce` |
| `Complaint` | yes | yes | `spam complaint` |
| `Invalid` | yes | yes | `invalid address` |
| `Manual` | no | yes | `suppressed by an operator` |
| `Unsubscribed` | no | **no** | `unsubscribed` |

`SuppressionList` has four required methods: `record`, `lookup`, `release(address, force)` and
`list(cursor, limit)`. `check(&rendered)` is provided and is what `Suppressing` calls. `release`
takes `force` because releasing a hard bounce is something an operator should have to mean.

`MemorySuppressionList` ships. A production list is a table you back with your own implementation of
the trait, and it is deliberately not an ORM entity, because `moso-mail` does not depend on the ORM.

## Delivery webhooks

Every provider posts bounces and complaints back to you, and a bounce endpoint without signature
verification is an open door: anybody who finds the URL can suppress any address, which is a denial
of service against one user's account recovery. `WebhookVerifier::verify` therefore checks the
signature and parses nothing unless the check passes. There is no "verification disabled" mode,
because a verifier is constructed with its secret.

| Scheme | Where the signature is | Algorithm | Freshness window |
| --- | --- | --- | --- |
| `WebhookScheme::Mailgun` | the body's `signature` object | HMAC-SHA256 over `timestamp \|\| token` | 300 s |
| `WebhookScheme::Resend` | `svix-signature` plus `svix-id` and `svix-timestamp` | HMAC-SHA256 over `"{id}.{timestamp}.{body}"`, secret base64 after `whsec_` | 300 s |
| `WebhookScheme::Postmark` | HTTP basic password, or `x-postmark-token` | constant-time digest compare | none |
| `WebhookScheme::SendGrid` | `x-twilio-email-event-webhook-signature` and `-timestamp` | ECDSA P-256 SHA-256, secret is the base64 DER public key | signed, not range-checked |
| SES via `SnsVerifier` | the body's `Signature` field | RSA PKCS1 SHA-256 against a pinned public key | `SignatureVersion` must be `2` |

`SnsVerifier` does not fetch the `SigningCertURL` the message names. The key is pinned in your
configuration, because fetching would put a network round trip inside a synchronous check and make
that check depend on an attacker-influenced URL. SNS signature version 1 (SHA-1) is refused outright.

Verified events feed straight into the suppression list:

```rust
use moso_core::config::SecretString;
use moso_mail::WebhookVerifier as _;
use moso_mail::webhook::{SharedSecretVerifier, WebhookScheme};

let verifier =
    SharedSecretVerifier::new(WebhookScheme::Postmark, SecretString::new("hook-token"));
let mut headers = http::HeaderMap::new();
headers.insert("x-postmark-token", http::HeaderValue::from_static("hook-token"));

let payload = bytes::Bytes::from_static(
    br#"{"RecordType":"Bounce","Type":"HardBounce","Email":"gone@example.com",
         "Description":"550 user unknown"}"#,
);

let events = verifier.verify(&headers, &payload).expect("the signature verifies");
assert_eq!(events.len(), 1);

let list = Arc::new(MemorySuppressionList::new());
assert_eq!(
    moso_mail::apply_events(list.as_ref(), &events).await.expect("applies"),
    1,
);

let inner = Arc::new(MemoryMailer::new());
let mailer = moso_mail::backend::Suppressing::new(inner.clone(), list);
assert!(mailer.send(&welcome("gone@example.com")).await.is_err());
assert!(inner.sent().is_empty());

// A forged webhook changes nothing.
let mut forged = http::HeaderMap::new();
forged.insert("x-postmark-token", http::HeaderValue::from_static("guessed"));
assert!(verifier.verify(&forged, &payload).is_err());
```

The handler that wires this up reads the raw body, because a re-serialised body does not verify.

```rust title="src/routes/webhooks.rs"
use moso::prelude::*;
use moso::deps::http::HeaderMap;
use moso::extract::Bytes;
use moso::response::NoContent;
use moso_mail::{SuppressionList, WebhookVerifier, apply_events};

/// Record bounces and complaints reported by the provider.
#[endpoint]
async fn delivery(
    headers: HeaderMap,
    Inject(verifier): Inject<dyn WebhookVerifier>,
    Inject(list): Inject<dyn SuppressionList>,
    Bytes(body): Bytes,
) -> Result<NoContent> {
    let events = verifier.verify(&headers, &body)?;
    apply_events(list.as_ref(), &events).await?;
    Ok(NoContent)
}
```

`WebhookEventKind` covers `Accepted`, `Delivered`, `SoftBounce`, `HardBounce`, `Complaint`,
`Unsubscribed`, `Invalid`, `Opened`, `Clicked` and `Deferred`. `suppresses()` maps the ones that
imply a suppression onto a reason, and `apply_events` returns how many it recorded. A soft bounce
records nothing, which is the intent: one deferred message is not a dead address.

`ProviderMailer::webhook_verifier(signing_secret)` builds the right verifier for the provider you
configured, so the two halves cannot drift apart.

## Staging safety

`Redirecting::new(inner, to)` sends every message to one address and preserves the real recipients
in an `X-Moso-Original-To` header (`moso_mail::backend::ORIGINAL_TO`). Set `redirect_to` in
`MailConfig` and `build` wires it for you, under the suppression wrapper.

Both `Suppressing` and `Redirecting` are always compiled. They are not behind a feature, so they
work in a build with no backend features at all.

## Testing

`MemoryMailer` is the double. It captures what was sent and can be made to fail.

```rust
let mailer = MemoryMailer::new();
mailer.set_from(Some(Address::new("hello@shop.example").expect("valid")));

// assert_none_sent
assert_eq!(mailer.sent_count(), 0);

mailer.send(&welcome("ada@example.com")).await.expect("sends");
mailer.send(&welcome("grace@example.com")).await.expect("sends");

// assert_sent::<WelcomeEmail>(2)
assert_eq!(mailer.count_of::<WelcomeEmail>(), 2);
assert_eq!(mailer.sent_of::<WelcomeEmail>().len(), 2);
// Either spelling of the name finds the same messages.
assert_eq!(mailer.sent_of_kind("WelcomeEmail").len(), 2);
assert_eq!(mailer.sent_of_kind(std::any::type_name::<WelcomeEmail>()).len(), 2);
assert_eq!(mailer.count_of_kind("PasswordReset"), 0);

let last: &RenderedEmail = &mailer.sent()[1];
assert_eq!(last.to[0].address(), "grace@example.com");
assert!(last.html.contains("verify"));
assert_eq!(last.from.address(), "hello@shop.example");
assert!(!last.text.is_empty(), "a text part is never optional");

mailer.clear();
assert_eq!(mailer.sent_count(), 0);
```

The seam, in full:

| Method | What it is for |
| --- | --- |
| `sent() -> Vec<RenderedEmail>` | everything sent, in order |
| `sent_count() -> usize` | the count, without cloning (`assert_none_sent`) |
| `sent_of::<T: Email + ?Sized>() -> Vec<RenderedEmail>` | everything written as the type `T` |
| `sent_of_kind(&str) -> Vec<RenderedEmail>` | the same, by name |
| `count_of::<T: Email + ?Sized>() -> usize` | the count `assert_sent::<T>(n)` compares |
| `count_of_kind(&str) -> usize` | the same, by name |
| `clear()` | forget everything sent |
| `fail_with(Option<&str>)` | make the next sends fail, retryably |
| `delay(Option<Duration>)` | make the next sends stall |
| `set_from(Option<Address>)` | the sender filled into a message that set none |
| `timeout(Duration) -> Self` | the send deadline, at construction |

`sent_of::<T>()` matches `T` by its Rust path, which is what `Email::kind` defaults to; a message
type that *overrides* `kind` is looked up with `sent_of_kind`, which matches either the short name
or the fully qualified one. Every method except `timeout` takes `&self`, so the mailer can stay
behind the `Arc<dyn Mailer>` the application injected.

`fail_with(Some("detail"))` makes the next sends fail with a retryable `Error::Unavailable`, which
is how you test a job's retry path. `delay(Some(..))` longer than `timeout(..)` produces
`Error::Timeout` instead, which is how you test the same path against a provider that stopped
answering, with no socket, and without waiting:

```rust
let mailer = MemoryMailer::new().timeout(Duration::from_millis(20));
mailer.delay(Some(Duration::from_secs(60)));

let error = mailer.send(&welcome("ada@example.com")).await.expect_err("times out");
assert!(matches!(error, moso_mail::Error::Timeout { .. }));
assert!(error.retryable());
assert_eq!(mailer.sent_count(), 0, "nothing was recorded as delivered");
```

Swap the mailer in a test app with `override_provider_dyn`. See [testing](./testing.md).

## Failure modes

`moso_mail::Error` converts into the framework's [error model](./errors.md), so every failure
becomes an RFC 9457 problem with the right status.

| Variant | Status | Retryable | When |
| --- | --- | --- | --- |
| `Suppressed` | 422, pointer `/to` | no | The recipient is on the suppression list |
| `Address` | 422, pointer `/to` | no | An address did not parse |
| `Template` | 500 | no | A template failed to parse or render |
| `Rejected` | 500 | no | The provider refused the message permanently |
| `Unavailable` | 503 | **yes** | 429, a 5xx, or a transport failure |
| `Timeout` | 504 | **yes** | The send did not finish inside `MailConfig::timeout` |
| `Unsupported` | 500 | no | The backend does not implement the operation |
| `Signature` | 401 | no | A webhook signature did not verify |
| `Config` | 500 | no | A configuration contradiction, including marketing mail with no unsubscribe header |

`error.retryable()` is true for `Unavailable` and `Timeout` and nothing else, and that is what a
job's retry policy should branch on. `error.is_suppressed()` distinguishes the one case you
probably want to swallow rather than retry. `error.backend()` names the backend for the five
variants that came from one.

Other things that surprise people:

- A message with no `from` and a backend that forgets to substitute the configured default renders
  as `unset@sender.invalid`. The `.invalid` TLD is reserved by RFC 2606, so the failure is obvious
  rather than plausible. Every shipped backend substitutes.
- `Bcc` never appears in the transmitted bytes. It travels in the SMTP envelope, built from
  `RenderedEmail::recipients()`.
- `Jinja` is strict about undefined variables. A template loaded from disk that references a
  variable your context does not set fails the render.
- Autoescaping keys off the file extension, so a template named without `.html` will not escape.
  Name HTML templates `.html`.
- `MailBackendKind::parse` trims and lowercases, and accepts the nine names in
  `MailBackendKind::NAMES`.

## See also

- [Background jobs](./jobs.md) for the queue that `RenderedEmail` was designed to travel through.
- [Dependency injection](./dependency-injection.md) for `provide_dyn` and `Inject<dyn Mailer>`.
- [Configuration](./configuration.md) for getting `MAIL_*` values into `MailConfig`.
- [Errors](./errors.md) for what the statuses above mean on the wire.
- [File storage](./file-storage.md), the other battery with the same shape.
