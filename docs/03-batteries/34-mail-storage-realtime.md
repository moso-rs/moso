# 34 - Mail, Object Storage & Realtime

> ⛔ **NOT IMPLEMENTED**, with two partial exceptions. `Mailer`, `Storage`, `Bus` and rate limiting
> do not exist. **Server-Sent Events do**: `moso::response::Sse` / `Event` are built in `moso-core`
> and document themselves as `text/event-stream`. The `ws` cargo feature exposes Axum's WebSocket
> upgrade surface, and Moso adds nothing on top of it. `Slot::RateLimit` is a reserved, empty slot.
> See [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

Three smaller batteries, each following the same pattern: a Moso-owned trait, pluggable backends, a
dev backend requiring no external service, and first-class testing support.

---

# Mail

## The trait

```rust
// spec - moso-mail
#[async_trait]
pub trait Mailer: Send + Sync + 'static {
    async fn send(&self, msg: &dyn Email) -> Result<MessageId>;
    async fn send_batch(&self, msgs: &[&dyn Email]) -> Result<Vec<Result<MessageId>>>;
    fn capabilities(&self) -> MailCapabilities;   // batching, templates, tracking, attachments
}

pub trait Email: Send + Sync {
    fn to(&self) -> Vec<Address>;
    fn subject(&self) -> String;
    fn html(&self) -> Result<String>;
    fn text(&self) -> Result<String>;             // required - never send HTML-only
    fn headers(&self) -> HeaderMap { HeaderMap::new() }
    fn attachments(&self) -> Vec<Attachment> { vec![] }
    fn tags(&self) -> Vec<(&str, &str)> { vec![] }   // for provider-side analytics
}
```

## Writing an email

```rust
// example - src/mail/welcome.rs
#[derive(Email)]
#[email(
    subject = "Welcome to {{ app_name }}, {{ user.name }}",
    html = "emails/welcome.html",
    text = "emails/welcome.txt",       // auto-generated from HTML if omitted
    from = "Shop <hello@shop.example>",
    tag("kind", "welcome"),
)]
pub struct WelcomeEmail<'a> {
    pub user: &'a User,
    pub verify_url: Url,
}
```

Templates use `minijinja` (Jinja2-compatible, so designers and LLMs already know it). The derive
checks at compile time that every variable referenced in the template exists on the struct -
a runtime "undefined variable" in a transactional email is a bad way to find a typo.

## Backends

| Backend | Feature | Use |
| --- | --- | --- |
| `console` | default in dev | prints to the terminal **and** serves a preview at `/_mail` with a rendered inbox |
| `file` | dev/CI | writes `.eml` files |
| `memory` | tests | assertable via `app.mail()` |
| `smtp` | `mail-smtp` | via `lettre`; pooled, STARTTLS/implicit TLS, DSN parsing |
| `ses`, `sendgrid`, `postmark`, `resend`, `mailgun` | per-provider | REST APIs, batch send, webhook signature verification |

The dev preview inbox is the highest-value 200 lines in this crate: seeing the rendered email in a
browser without configuring SMTP removes a real friction point.

## Operational details

- **Sending happens in a job by default.** `mail.send()` from a handler enqueues; `mail.send_now()`
  blocks. Inline SMTP in a request handler is a latency and reliability trap.
- Retries and the DLQ come free from `moso-jobs`.
- **Suppression list**: hard bounces and complaints are recorded (via provider webhooks, whose
  signatures Moso verifies) and suppressed automatically. Sending to a suppressed address returns
  `Error::Suppressed` rather than damaging domain reputation.
- **Idempotency**: a `message_key` prevents duplicate sends on job retry.
- Unsubscribe headers (`List-Unsubscribe`, `List-Unsubscribe-Post`) for anything marked
  `#[email(marketing)]`.
- Docs cover SPF/DKIM/DMARC setup, because the framework's job is to keep the mail out of spam.

## Testing

```rust
app.mail().assert_sent::<WelcomeEmail>(1);
app.mail().last::<WelcomeEmail>().assert_to("a@b.com").assert_html_contains("verify");
app.mail().assert_none_sent();
```

---

# Object storage

## The trait

```rust
// spec - moso-storage
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    async fn put(&self, key: &StorageKey, body: ByteStream, opts: PutOpts) -> Result<ObjectMeta>;
    async fn get(&self, key: &StorageKey) -> Result<ByteStream>;
    async fn get_range(&self, key: &StorageKey, range: Range<u64>) -> Result<ByteStream>;
    async fn head(&self, key: &StorageKey) -> Result<Option<ObjectMeta>>;
    async fn delete(&self, key: &StorageKey) -> Result<bool>;
    async fn list(&self, prefix: &str, cursor: Option<String>) -> Result<Listing>;
    async fn copy(&self, from: &StorageKey, to: &StorageKey) -> Result<ObjectMeta>;

    /// Stream the object back, `Range` and `ETag` already handled.
    async fn serve(&self, key: &StorageKey) -> Result<ServedObject>;

    /// Time-limited direct-download URL.
    async fn signed_url(&self, key: &StorageKey, ttl: Duration) -> Result<Url>;
    /// Direct-to-storage upload, bypassing the app entirely.
    async fn presigned_upload(&self, key: &StorageKey, opts: UploadPolicy) -> Result<PresignedPost>;

    async fn multipart_start(&self, key: &StorageKey, opts: PutOpts) -> Result<MultipartUpload>;
}
```

Backends: `local` (dev, with a served route), `s3` (and any S3-compatible: R2, MinIO, Backblaze,
Wasabi, Tigris), `gcs`, `azure`, `memory` (tests).

Signing, per backend: S3 signs and presigns with SigV4; GCS does both with a V4 signed URL when it
holds a service-account key (and mints its own access tokens from the same key, RS256), and neither
under workload identity or a supplied bearer token, because neither holds a private key; Azure does
both with a service SAS; `local` signs only once `served_at` gives it a route to check the signature
against; `memory` never does. Every signature is `ring` - HMAC-SHA256, or RSA PKCS#1 v1.5 over
SHA-256 seeded from the OS CSPRNG - and what the crate contains is the canonical strings.

## Typed attachments on entities

```rust
// example
#[derive(Entity)]
pub struct Product {
    #[entity(pk)] pub id: Id<Product>,
    #[entity(attachment(variants(thumb = "200x200", card = "600x400"), accept = "image/*",
                        max_size = "10MiB"))]
    pub image: Attachment<Image>,
}

// usage - as built. The descriptor is a column value, so the store and the TTL
// are arguments rather than fields: a storage handle inside it could not be
// serialised into `jsonb`, and a signature minted when the row was *written*
// would have expired before anything read it.
product.image.url(&storage, &Variant::ORIGINAL, ttl).await?;
product.image.url(&storage, &Variant::new("thumb"), ttl).await?;
Attachment::attach(upload, &storage, "products/prd_1").await?;   // the DB write is yours
```

Variants are generated in a background job (`moso-jobs`), so the request does not wait on image
processing. The column stores a JSON descriptor (key, size, content type, checksum, variant state),
so no extra table is needed.

**No image codec is a dependency, and none is planned.** `moso-storage` owns the seam -
`VariantSpec` declares the work, `Attachment::read_original` and `store_variant` move the bytes,
`Rendition` is the handover, `mark_failed` records what will never succeed - and the encoder is the
application's. The crate's rustdoc carries a tested job body that proves the wiring with a
byte-identity transform.

## Upload path

```rust
// example - streamed straight to the backend, never buffered in memory
#[endpoint]
async fn upload(
    Inject(storage): Inject<dyn Storage>,
    Upload(file): Upload<ProductImage>,     // typed multipart: validates type, size, dimensions
) -> Result<Created<AssetOut>> { … }
```

`Upload<T>` validates content type by **sniffing magic bytes**, not by trusting the declared type
or the extension; enforces size limits while streaming; rejects on the first offending byte rather
than after buffering; strips EXIF from images by default (a privacy default that matters -
uploaded photos carry GPS coordinates); and re-encodes SVGs through a sanitiser or refuses them,
since SVG is an XSS vector.

For large files the docs push presigned direct upload (`presigned_upload`) so bytes never traverse
the application, with a completion callback that validates and records the object.

## Serving

`storage.serve(key)` returns a `ServedObject` response with Range support, ETag/If-None-Match,
correct `Content-Disposition`, and `Content-Security-Policy: sandbox` for user-uploaded content.
The free function `moso_storage::serve(storage, key)` is the same operation - it delegates to the
method, which is where the implementation lives. The docs strongly recommend serving user content
from a separate origin and explain why (cookie and same-origin isolation).

## Deadlines

Two, because a single number gets both cases wrong. `StorageConfig::timeout` bounds every call that
**answers once** - `head`, `delete`, `list`, `copy`, `signed_url`, `presigned_upload`,
`multipart_start`, `probe`, and each multipart part - and `StorageConfig::stall_timeout` bounds
`put`, `get` and `get_range` by how long they may move **no bytes**, restarting on every chunk. A
whole-operation deadline around a streaming transfer kills healthy gibibytes; a stall deadline
around a `head` never fires. `StorageConfig::build` wraps whatever it builds in `TimedStorage`, so
the policy is enforced without an application doing anything, and the two failures are separate
values - `Error::Timeout` and `Error::Stalled`, both retryable, both a 504.

---

# Realtime: WebSockets & SSE

## Server-Sent Events

```rust
// example
#[endpoint]
async fn notifications(
    Depends(CurrentUser(user)): Depends<CurrentUser>,
    Inject(bus): Inject<Bus>,
    Inject(shutdown): Inject<shutdown::Signal>,
) -> Result<Sse<impl Stream<Item = Result<Event>>>> {
    let stream = bus.subscribe::<Notification>(Topic::user(user.id))
        .map(|n| Event::json("notification", &n))
        .take_until(shutdown.recv());
    Ok(Sse::new(stream).keep_alive(Duration::from_secs(15)))
}
```

SSE is the default recommendation for server→client push: it works through proxies, reconnects
automatically, and needs no special infrastructure. `Sse` handles `Last-Event-ID` resumption when
the stream is backed by a replayable source.

## WebSockets

```rust
// example
#[endpoint]
async fn chat(ws: WebSocketUpgrade, Depends(CurrentUser(user)): Depends<CurrentUser>)
    -> Result<Response>
{
    Ok(ws.protocols(["chat.v1"]).on_upgrade(move |socket| chat_session(socket, user)))
}
```

Wraps Axum's WebSocket support and adds: authentication **before** upgrade (so an unauthenticated
socket is never established), per-connection rate limiting, a message size cap, automatic
ping/pong with a dead-peer timeout, graceful close on shutdown with a status code, and connection
metrics. Typed messages via `WebSocket<ClientMsg, ServerMsg>` where both derive `Schema`, giving
compile-time-checked protocol handling and an `x-websocket` OpenAPI extension documenting it.

## The `Bus` - cross-instance pub/sub

```rust
// spec
pub trait Bus: Send + Sync + 'static {
    fn publish<T: Topic>(&self, topic: T, msg: &T::Message) -> impl Future<Output = Result<()>> + Send;
    fn subscribe<T: Topic>(&self, topic: T) -> impl Stream<Item = T::Message> + Send;
}
```

Backends: in-process (dev/single instance), Redis pub/sub, Postgres `LISTEN/NOTIFY`. This is the
piece people always have to build themselves to make WebSockets work across more than one pod, and
it is ~300 lines that unlocks realtime for a multi-instance deployment.

Presence tracking (`bus.presence(topic)`) is built on KV with TTL heartbeats.

## Acceptance criteria (WP-21)

**Mail:** template variable checked at compile time; console backend renders a browsable preview;
suppression prevents send; provider webhook signatures verified; `app.mail()` assertions work.
**Storage:** a 1 GiB upload streams with < 20 MiB peak RSS; magic-byte sniffing rejects a
`.png`-named executable; EXIF stripped; presigned upload round-trips; Range requests correct.
**Realtime:** SSE survives a 10-minute idle through a proxy with keep-alive; WS auth rejects
pre-upgrade; a message published on instance A reaches a subscriber on instance B within 50 ms;
all sockets close cleanly within the shutdown grace.
