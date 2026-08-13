//! Cross-instance publish/subscribe, and presence.
//!
//! # Why this lives in `moso-kv`
//!
//! A [`Bus`] is a typed layer over [`KvStore::publish`](crate::KvStore::publish)
//! and [`KvStore::subscribe`](crate::KvStore::subscribe), exactly as
//! [`Kv`](crate::Kv) is a typed layer over the rest of the store — and
//! [`Presence`] is a set of keys with TTL heartbeats, which is a KV structure
//! and nothing else. The three backends a bus needs (in-process, Redis pub/sub,
//! PostgreSQL `LISTEN`/`NOTIFY`) are the three backends this crate already has,
//! wired to the same connection pool and the same circuit breaker.
//!
//! Putting it in `moso-core` instead would have pulled a Redis client and a
//! PostgreSQL driver into the crate every Moso application compiles, to serve a
//! feature most of them do not use. This module costs a `moso-kv` dependency,
//! which a realtime application has anyway.
//!
//! # What it unlocks
//!
//! WebSockets and SSE across more than one process. A socket is held by whichever
//! instance the load balancer picked; the event that should reach it happens on
//! another. This is the piece everyone has to build themselves to make realtime
//! work on more than one pod.
//!
//! ```text
//! #[endpoint]
//! async fn notifications(
//!     Depends(CurrentUser(user)): Depends<CurrentUser>,
//!     Inject(bus): Inject<dyn Bus>,
//!     Inject(shutdown): Inject<shutdown::Signal>,
//! ) -> Result<Sse<impl Stream<Item = Result<Event>>>> {
//!     let stream = bus.subscribe(&UserNotifications(user.id)).await?
//!         .map(|n| Event::json("notification", &n))
//!         .take_until(shutdown.recv());
//!     Ok(Sse::new(stream).keep_alive(Duration::from_secs(15)))
//! }
//! ```

use std::borrow::Cow;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use moso_core::BoxFuture;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{MessageStream, Result};

/// The separator between a topic's name and its instance key.
///
/// ```
/// assert_eq!(moso_kv::bus::TOPIC_SEPARATOR, ':');
/// ```
pub const TOPIC_SEPARATOR: char = ':';

/// A typed channel: a name, an instance, and the message that travels on it.
///
/// The instance key is what makes one topic type serve every user:
/// `UserNotifications(user_id)` is a distinct channel per user, with one
/// `Message` type and one place the shape is defined.
///
/// ```
/// use moso_kv::Topic;
/// use serde::{Deserialize, Serialize};
///
/// /// What arrives in a user's notification feed.
/// #[derive(Serialize, Deserialize)]
/// pub struct Notification {
///     /// What to show.
///     pub text: String,
/// }
///
/// /// One user's notification feed.
/// pub struct UserNotifications(pub u64);
///
/// impl Topic for UserNotifications {
///     type Message = Notification;
///     const NAME: &'static str = "notifications";
///
///     fn instance(&self) -> std::borrow::Cow<'_, str> {
///         self.0.to_string().into()
///     }
/// }
///
/// assert_eq!(UserNotifications::NAME, "notifications");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a bus topic",
    label = "not a topic",
    note = "a topic carries a `Message` type, a `NAME`, and an `instance` that says which \
            channel of that name it is",
    note = "help: write `impl Topic for {Self}` with `type Message = …;`, \
            `const NAME: &'static str = \"…\";` and `fn instance(&self)`",
    note = "help: `Message` must be `Serialize + DeserializeOwned`, because it crosses a process \
            boundary — that is the whole point of a bus"
)]
pub trait Topic: Send + Sync + 'static {
    /// What travels on this channel.
    type Message: Serialize + DeserializeOwned + Send + 'static;

    /// The topic's name, shared by every instance: `"notifications"`.
    ///
    /// Together with [`instance`](Topic::instance) it forms the channel, so two
    /// topic types must not share a name.
    const NAME: &'static str;

    /// Which channel of that name: a user id, a room id, a tenant.
    ///
    /// `""` for a topic with exactly one channel — a global broadcast.
    fn instance(&self) -> Cow<'_, str>;

    /// The full channel name, `{NAME}:{instance}`.
    ///
    /// Provided, and not meant to be overridden: the shipped backends prefix it
    /// with the application name the same way [`Key`](crate::Key) does, so two
    /// applications on one Redis do not hear each other.
    fn channel(&self) -> String {
        let instance = self.instance();
        if instance.is_empty() {
            Self::NAME.to_owned()
        } else {
            format!("{}{TOPIC_SEPARATOR}{instance}", Self::NAME)
        }
    }
}

/// What a bus backend can do.
///
/// ```
/// use moso_kv::BusCapabilities;
///
/// let caps = BusCapabilities::in_process();
/// assert!(!caps.cross_process);
/// assert!(!caps.replay);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BusCapabilities {
    /// Whether a publish on one process reaches a subscriber on another.
    ///
    /// `false` for the in-process backend, which is honest rather than
    /// convenient: a test that passes against it and fails on two pods is worse
    /// than a capability flag.
    pub cross_process: bool,
    /// Whether a subscriber can ask for messages it missed while disconnected.
    ///
    /// What `Last-Event-ID` resumption needs. Redis pub/sub cannot;
    /// a stream-backed or table-backed bus can.
    pub replay: bool,
    /// Whether `*`-style pattern subscriptions work.
    pub patterns: bool,
    /// Whether the backend tracks presence.
    pub presence: bool,
    /// The largest message the backend accepts, in bytes.
    pub max_message_bytes: usize,
}

impl BusCapabilities {
    /// The in-process backend's capabilities: everything except crossing a
    /// process boundary.
    ///
    /// ```
    /// use moso_kv::BusCapabilities;
    ///
    /// assert!(BusCapabilities::in_process().patterns);
    /// ```
    #[must_use]
    pub const fn in_process() -> Self {
        Self {
            cross_process: false,
            replay: false,
            patterns: true,
            presence: true,
            max_message_bytes: 1024 * 1024,
        }
    }
}

impl Default for BusCapabilities {
    fn default() -> Self {
        Self::in_process()
    }
}

/// Cross-instance publish/subscribe.
///
/// Dyn-compatible (decision D4): an application injects `Inject<dyn Bus>` and
/// never names the backend, so the same handler works in-process in a test and
/// over Redis in production. The typed API is [`TypedBus`], which every `Bus`
/// gets for free — including `dyn Bus`.
///
/// ```no_run
/// use bytes::Bytes;
/// use moso_kv::Bus;
///
/// async fn ping(bus: &dyn Bus) -> moso_kv::Result<u64> {
///     bus.publish_raw("heartbeat", Bytes::from_static(b"ok")).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a message bus",
    label = "not a bus backend",
    note = "a bus backend is `Send + Sync + 'static` and implements `name`, `capabilities`, \
            `publish_raw` and `subscribe_raw`",
    note = "help: use a shipped backend — `LocalBus` for a single process, `KvBus` over any \
            `moso_kv::Kv` for Redis pub/sub or PostgreSQL LISTEN/NOTIFY",
    note = "help: the typed `publish` and `subscribe` come from `TypedBus`, which every `Bus` \
            gets automatically — implement this trait, not that one"
)]
pub trait Bus: Send + Sync + 'static {
    /// The backend's name, for logs and metrics.
    fn name(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> BusCapabilities;

    /// Publish bytes on a channel, returning how many subscribers received it.
    ///
    /// The count is best-effort and backend-specific: Redis reports how many
    /// *connections* were subscribed at publish time, which is not the same as
    /// how many sockets will see it. Useful as a metric, never as a delivery
    /// receipt.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the backend cannot be reached.
    fn publish_raw<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>>;

    /// Subscribe to a channel.
    ///
    /// The stream ends when the backend's connection ends, so a caller that
    /// needs to survive a reconnect should resubscribe — the shipped backends
    /// do that internally and the stream survives.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the backend cannot be reached.
    fn subscribe_raw<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>>;

    /// Subscribe to every channel matching a glob pattern.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error) when
    /// [`BusCapabilities::patterns`] is false.
    fn subscribe_pattern<'a>(&'a self, pattern: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        let _ = pattern;
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend: self.name(),
                operation: "subscribe_pattern",
                capability: "patterns",
            })
        })
    }

    /// Presence tracking, when this backend has it.
    ///
    /// `None` rather than a method that fails, so a caller can branch on
    /// availability without catching an error.
    fn presence(&self) -> Option<&dyn Presence> {
        None
    }

    /// A readiness probe.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the backend cannot be reached.
    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        // A bus with no remote is always reachable. `KvBus` overrides this;
        // leaving the default in place for a remote backend would be the
        // dishonest choice.
        Box::pin(async { Ok(()) })
    }
}

/// The typed half of the bus: [`publish`](TypedBus::publish) and
/// [`subscribe`](TypedBus::subscribe) over a [`Topic`].
///
/// A separate trait because generic methods would make [`Bus`] dyn-incompatible,
/// and the trait object is what an application injects. The blanket impl covers
/// `dyn Bus` as well as every concrete backend, so nothing is lost.
///
/// ```no_run
/// use moso_kv::{Bus, Topic, TypedBus};
///
/// async fn send<T: Topic>(bus: &dyn Bus, topic: &T, message: &T::Message)
///     -> moso_kv::Result<u64>
/// {
///     bus.publish(topic, message).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot publish typed messages",
    label = "not a bus",
    note = "`TypedBus` is implemented for every `Bus`, including `dyn Bus` — so this usually \
            means `{Self}` does not implement `Bus`",
    note = "help: implement `Bus` on it, or use one of the shipped backends"
)]
pub trait TypedBus: Bus {
    /// Publish a message on a topic.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the backend cannot be reached,
    /// or a serialisation failure — which is a programming error, and is
    /// reported rather than swallowed so it does not become a silently missing
    /// notification.
    fn publish<'a, T: Topic>(
        &'a self,
        topic: &'a T,
        message: &'a T::Message,
    ) -> impl Future<Output = Result<u64>> + Send + 'a;

    /// Subscribe to a topic, decoding each message.
    ///
    /// A message that fails to decode is **skipped and counted**, not returned
    /// as an error: one bad publish from an older deploy must not end a
    /// subscriber's stream and disconnect every socket behind it.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the backend cannot be reached.
    fn subscribe<'a, T: Topic>(
        &'a self,
        topic: &'a T,
    ) -> impl Future<Output = Result<TopicStream<T>>> + Send + 'a;
}

// Every bus gets the typed API, `dyn Bus` included — which is what lets an
// application inject `Inject<dyn Bus>` and still write `bus.publish(&topic, &m)`.
// `do_not_recommend` keeps a failed `Bus` bound from being reported as "consider
// implementing `TypedBus`", which is the half nobody writes.
#[diagnostic::do_not_recommend]
impl<B: Bus + ?Sized> TypedBus for B {
    fn publish<'a, T: Topic>(
        &'a self,
        topic: &'a T,
        message: &'a T::Message,
    ) -> impl Future<Output = Result<u64>> + Send + 'a {
        // Serialised *before* the async block: `T::Message` is `Send` but not
        // necessarily `Sync`, so holding a `&T::Message` across an await would
        // make the returned future `!Send` — and the whole point of this trait
        // is that it works behind `Inject<dyn Bus>` in a spawned task.
        let encoded = serde_json::to_vec(message)
            .map(Bytes::from)
            .map_err(|source| {
                // A message that will not serialise is a programming error, and
                // swallowing it here is how a notification silently never arrives.
                crate::Error::codec(T::NAME, Box::new(source))
            });

        async move {
            let payload = encoded?;
            let limit = self.capabilities().max_message_bytes;
            if payload.len() > limit {
                return Err(crate::Error::Channel {
                    backend: self.name(),
                    channel: topic.channel(),
                    reason: "the message is larger than this backend accepts; publish an \
                             identifier and let the subscriber fetch the body",
                });
            }

            self.publish_raw(&topic.channel(), payload).await
        }
    }

    // `async fn` is not available here: the trait declares the method as
    // returning `impl Future + Send + 'a`, and the explicit `Send` bound is
    // what lets a subscription be held by a spawned task.
    #[expect(
        clippy::manual_async_fn,
        reason = "the `+ Send` bound on the returned future is load-bearing"
    )]
    fn subscribe<'a, T: Topic>(
        &'a self,
        topic: &'a T,
    ) -> impl Future<Output = Result<TopicStream<T>>> + Send + 'a {
        async move {
            let inner = self.subscribe_raw(&topic.channel()).await?;
            Ok(TopicStream {
                inner,
                skipped: 0,
                marker: core::marker::PhantomData,
            })
        }
    }
}

/// A stream of decoded messages from one topic.
///
/// ```no_run
/// use futures_util::StreamExt;
/// use moso_kv::{Topic, TopicStream};
///
/// async fn first<T: Topic>(mut stream: TopicStream<T>) -> Option<T::Message> {
///     stream.next().await
/// }
/// ```
pub struct TopicStream<T: Topic> {
    /// The undecoded messages.
    inner: MessageStream,
    /// How many messages failed to decode and were skipped.
    skipped: u64,
    /// The topic, which holds no data here.
    marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: Topic> TopicStream<T> {
    /// How many messages failed to decode and were skipped.
    ///
    /// Exposed as `moso_bus_decode_errors_total` too. A non-zero value means a
    /// publisher and a subscriber disagree about the message's shape, which is
    /// a deploy-ordering problem worth seeing.
    ///
    /// ```no_run
    /// # use moso_kv::{Topic, TopicStream};
    /// # fn f<T: Topic>(s: &TopicStream<T>) { let _: u64 = s.skipped(); }
    /// ```
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}

impl<T: Topic> Stream for TopicStream<T> {
    type Item = T::Message;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T::Message>> {
        // `TopicStream` is `Unpin` in everything that matters — `MessageStream`
        // is already a `Pin<Box<..>>` and `PhantomData` and `u64` are `Unpin` —
        // so the projection is a plain field borrow.
        let this = self.get_mut();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(bytes)) => {
                    match serde_json::from_slice::<T::Message>(&bytes) {
                        Ok(message) => return Poll::Ready(Some(message)),
                        Err(error) => {
                            // Counted and skipped, not returned as an error: one
                            // bad publish from an older deploy must not end this
                            // stream and disconnect every socket behind it.
                            this.skipped = this.skipped.saturating_add(1);
                            tracing::warn!(
                                target: "moso::bus",
                                topic = T::NAME,
                                skipped = this.skipped,
                                error = %error,
                                "a message on this topic did not decode and was skipped",
                            );
                        }
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<T: Topic> core::fmt::Debug for TopicStream<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TopicStream")
            .field("topic", &T::NAME)
            .field("skipped", &self.skipped)
            .finish()
    }
}

/// Who is currently connected to a channel.
///
/// Built on keys with TTL heartbeats: a member is present while it keeps
/// refreshing, and disappears on its own when a process dies. There is no
/// "leave" message to lose, which is what makes presence survive a crash rather
/// than accumulating ghosts.
///
/// ```no_run
/// use moso_kv::Presence;
///
/// async fn who(presence: &dyn Presence, channel: &str) -> moso_kv::Result<Vec<String>> {
///     presence.members(channel).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot track presence",
    label = "not a presence tracker",
    note = "a presence tracker implements `join`, `heartbeat`, `leave` and `members`",
    note = "help: reach it through `Bus::presence()` rather than implementing it — the shipped \
            backends build it on keys with TTL heartbeats, so a crashed process expires instead \
            of leaving a ghost"
)]
pub trait Presence: Send + Sync + 'static {
    /// Record a member as present, for `ttl`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the store cannot be reached.
    fn join<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>>;

    /// Extend a member's presence. Called on a timer by whoever holds the
    /// connection.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the store cannot be reached.
    fn heartbeat<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>>;

    /// Remove a member now, rather than waiting for the TTL.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the store cannot be reached.
    fn leave<'a>(&'a self, channel: &'a str, member: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Who is present, as of now.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the store cannot be reached.
    fn members<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<Vec<String>>>;

    /// How many are present. Cheaper than [`members`](Presence::members) on
    /// every backend that can count without listing.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error) when the store cannot be reached.
    fn count<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { Ok(self.members(channel).await?.len() as u64) })
    }
}

/// A bus inside one process.
///
/// For development, for a single-instance deployment, and for tests. It reports
/// `cross_process: false`, so a test asserting cross-instance delivery against
/// it fails loudly instead of passing and then failing on two pods.
///
/// ```no_run
/// use moso_kv::LocalBus;
///
/// let bus = LocalBus::new();
/// assert_eq!(moso_kv::Bus::name(&bus), "local");
/// ```
#[derive(Debug, Default)]
pub struct LocalBus {
    /// One broadcast sender per channel, created on first subscribe.
    channels:
        std::sync::RwLock<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Bytes>>>,
    /// How many messages a slow subscriber may fall behind before it starts
    /// missing them. Bounded on purpose: an unbounded buffer turns a slow socket
    /// into a memory leak.
    buffer: usize,
    /// Members by channel, with their expiry.
    members: std::sync::RwLock<
        std::collections::HashMap<String, std::collections::HashMap<String, std::time::Instant>>,
    >,
}

impl LocalBus {
    /// A bus buffering 256 messages per subscriber.
    ///
    /// ```
    /// use moso_kv::LocalBus;
    ///
    /// assert_eq!(LocalBus::new().channel_count(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: std::sync::RwLock::new(std::collections::HashMap::new()),
            buffer: DEFAULT_BUFFER,
            members: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// How many messages a slow subscriber may fall behind.
    ///
    /// ```
    /// # use moso_kv::LocalBus;
    /// let _ = LocalBus::new().buffer(1024);
    /// ```
    #[must_use]
    pub fn buffer(mut self, messages: usize) -> Self {
        self.buffer = messages.max(1);
        self
    }

    /// How many channels currently have a subscriber.
    ///
    /// ```
    /// # use moso_kv::LocalBus;
    /// assert_eq!(LocalBus::new().channel_count(), 0);
    /// ```
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The sender for a channel, created on first use.
    fn sender(&self, channel: &str) -> tokio::sync::broadcast::Sender<Bytes> {
        if let Some(sender) = self
            .channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(channel)
        {
            return sender.clone();
        }
        self.channels
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(channel.to_owned())
            .or_insert_with(|| tokio::sync::broadcast::channel(self.buffer).0)
            .clone()
    }

    /// Every channel whose name matches a glob pattern.
    fn matching(&self, pattern: &str) -> Vec<String> {
        self.channels
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|channel| glob_matches(pattern, channel))
            .cloned()
            .collect()
    }
}

/// How many messages a slow subscriber may fall behind by default.
const DEFAULT_BUFFER: usize = 256;

/// Whether a glob pattern with `*` and `?` matches a channel name.
///
/// Redis' `PSUBSCRIBE` grammar, minus the character classes nobody uses in a
/// channel name. Written here rather than pulled in: it is fifteen lines and
/// the alternative is a regex engine on the subscribe path.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    let (mut p, mut n) = (0_usize, 0_usize);
    // Where to resume from if the `*` we are inside has to swallow one more
    // character. This is the standard backtracking match, and it is linear in
    // practice because there is at most one live star.
    let (mut star, mut resume) = (None, 0_usize);

    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some(p);
                p += 1;
                resume = n;
            }
            Some(b'?') => {
                p += 1;
                n += 1;
            }
            Some(byte) if *byte == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some(at) => {
                    p = at + 1;
                    resume += 1;
                    n = resume;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}

/// Turn a broadcast receiver into a [`MessageStream`].
///
/// A subscriber that falls behind by more than the buffer misses messages and
/// is told so in the log rather than having its stream end: a dropped
/// notification is better than a disconnected socket.
fn broadcast_stream(
    receiver: tokio::sync::broadcast::Receiver<Bytes>,
    channel: String,
) -> MessageStream {
    Box::pin(futures_util::stream::unfold(
        (receiver, channel),
        |(mut receiver, channel)| async move {
            loop {
                match receiver.recv().await {
                    Ok(bytes) => return Some((bytes, (receiver, channel))),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        // A dropped notification is better than a disconnected
                        // socket, so the stream continues and the gap is logged.
                        tracing::warn!(
                            target: "moso::bus",
                            channel = %channel,
                            missed,
                            "a subscriber fell behind and missed messages",
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    ))
}

impl Bus for LocalBus {
    fn name(&self) -> &'static str {
        "local"
    }

    fn capabilities(&self) -> BusCapabilities {
        BusCapabilities::in_process()
    }

    fn publish_raw<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            // `send` fails when there is no receiver, which is not an error:
            // publishing into an empty room is a no-op with a count of zero.
            Ok(self.sender(channel).send(payload).unwrap_or(0) as u64)
        })
    }

    fn subscribe_raw<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move {
            Ok(broadcast_stream(
                self.sender(channel).subscribe(),
                channel.to_owned(),
            ))
        })
    }

    fn subscribe_pattern<'a>(&'a self, pattern: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move {
            // Only channels that exist *now* are merged: a channel created
            // after the subscription began has no sender to attach to. That is
            // Redis' behaviour too for a pattern that spans a new channel
            // within one connection's lifetime, and pretending otherwise would
            // make correct code here break in production.
            let streams: Vec<MessageStream> = self
                .matching(pattern)
                .into_iter()
                .map(|channel| broadcast_stream(self.sender(&channel).subscribe(), channel))
                .collect();

            Ok(Box::pin(futures_util::stream::select_all(streams)) as MessageStream)
        })
    }

    fn presence(&self) -> Option<&dyn Presence> {
        Some(self)
    }
}

impl LocalBus {
    /// The member map, dropping anything whose lease has run out.
    ///
    /// Expiry happens on read rather than on a timer: there is no process to
    /// run the timer in, and a member nobody asks about costs nothing.
    fn sweep(
        &self,
    ) -> std::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<String, std::collections::HashMap<String, std::time::Instant>>,
    > {
        let mut members = self
            .members
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        for channel in members.values_mut() {
            channel.retain(|_, expiry| *expiry > now);
        }
        members.retain(|_, channel| !channel.is_empty());
        members
    }
}

impl Presence for LocalBus {
    fn join<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.sweep()
                .entry(channel.to_owned())
                .or_default()
                .insert(member.to_owned(), std::time::Instant::now() + ttl);
            Ok(())
        })
    }

    fn heartbeat<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>> {
        // A heartbeat for a member that has already expired re-joins it, which
        // is what a client resuming after a pause expects.
        self.join(channel, member, ttl)
    }

    fn leave<'a>(&'a self, channel: &'a str, member: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut members = self.sweep();
            if let Some(channel) = members.get_mut(channel) {
                channel.remove(member);
            }
            Ok(())
        })
    }

    fn members<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let members = self.sweep();
            let mut names: Vec<String> = members
                .get(channel)
                .map(|channel| channel.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            Ok(names)
        })
    }
}

/// A bus over any [`Kv`](crate::Kv), and therefore over Redis pub/sub or
/// PostgreSQL `LISTEN`/`NOTIFY`.
///
/// The cross-process one. Which transport it actually uses is whichever backend
/// the [`Kv`](crate::Kv) was built with, so a deployment moves between them by
/// changing a URL — and a `Kv` whose store reports no `pubsub` capability is
/// refused at construction rather than at the first publish.
///
/// ```no_run
/// use moso_kv::{Kv, KvBus};
///
/// # fn f(kv: Kv) -> moso_kv::Result<KvBus> {
/// KvBus::new(kv)
/// # }
/// ```
#[derive(Debug)]
pub struct KvBus {
    /// Where messages go, and where presence keys live.
    kv: crate::Kv,
    /// How long a presence entry lives without a heartbeat.
    presence_ttl: Duration,
}

/// How long a presence entry survives without a heartbeat, by default.
const DEFAULT_PRESENCE_TTL: Duration = Duration::from_secs(30);

/// The namespace presence keys live in.
const PRESENCE_NAMESPACE: &str = "presence";

/// The version of the presence key layout.
const PRESENCE_VERSION: u16 = 1;

impl KvBus {
    /// A bus over `kv`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error) when the store has no `pubsub`
    /// capability — checked here, at construction, so the failure is at boot and
    /// not at the first message.
    ///
    /// ```
    /// # use moso_kv::{Kv, KvBus};
    /// # fn f() -> moso_kv::Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let bus = KvBus::new(kv)?;
    /// assert_eq!(moso_kv::Bus::name(&bus), "memory");
    /// # Ok(()) }
    /// # f().expect("the memory store has pubsub");
    /// ```
    pub fn new(kv: crate::Kv) -> Result<Self> {
        if !kv.capabilities().pubsub {
            return Err(crate::Error::Unsupported {
                backend: kv.store().name(),
                operation: "subscribe",
                capability: "pubsub",
            });
        }
        Ok(Self {
            kv,
            presence_ttl: DEFAULT_PRESENCE_TTL,
        })
    }

    /// How long a presence entry survives without a heartbeat.
    ///
    /// Thirty seconds by default, with clients heartbeating every ten: a member
    /// can miss two heartbeats before disappearing, which is enough to survive a
    /// garbage-collection pause and not enough to leave a ghost for a minute.
    ///
    /// ```
    /// # use moso_kv::{Kv, KvBus};
    /// # fn f() -> moso_kv::Result<()> {
    /// let bus = KvBus::new(Kv::in_memory("shop")?)?.presence_ttl(
    ///     std::time::Duration::from_secs(45),
    /// );
    /// # let _ = bus;
    /// # Ok(()) }
    /// # f().expect("builds");
    /// ```
    #[must_use]
    pub fn presence_ttl(mut self, ttl: Duration) -> Self {
        self.presence_ttl = ttl;
        self
    }

    /// The store this bus publishes through.
    ///
    /// ```
    /// # use moso_kv::{Kv, KvBus};
    /// # fn f() -> moso_kv::Result<()> {
    /// let bus = KvBus::new(Kv::in_memory("shop")?)?;
    /// assert_eq!(bus.kv().app(), "shop");
    /// # Ok(()) }
    /// # f().expect("builds");
    /// ```
    #[must_use]
    pub fn kv(&self) -> &crate::Kv {
        &self.kv
    }

    /// The channel a topic actually publishes on.
    ///
    /// Prefixed with the application name the same way [`Key`](crate::Key) is,
    /// so two applications sharing one Redis do not hear each other. Exposed
    /// because an operator debugging with `redis-cli PSUBSCRIBE` needs to know
    /// the real name.
    ///
    /// ```
    /// # use moso_kv::{Kv, KvBus};
    /// # fn f() -> moso_kv::Result<()> {
    /// let bus = KvBus::new(Kv::in_memory("shop")?)?;
    /// assert_eq!(bus.qualified("notifications:7"), "moso:v1:shop:notifications:7");
    /// # Ok(()) }
    /// # f().expect("builds");
    /// ```
    #[must_use]
    pub fn qualified(&self, channel: &str) -> String {
        format!("moso:v1:{}:{channel}", self.kv.app())
    }

    /// The key one member's presence lease lives at.
    fn member_key(&self, channel: &str, member: &str) -> Result<crate::Key> {
        let mut buf = crate::key::KeyBuf::new(self.kv.app(), PRESENCE_NAMESPACE, PRESENCE_VERSION)?;
        buf.segment_str(channel);
        buf.segment_str(member);
        Ok(buf.finish()?)
    }

    /// The prefix every member of one channel sits under.
    fn channel_prefix(&self, channel: &str) -> Result<crate::Key> {
        let mut buf = crate::key::KeyBuf::new(self.kv.app(), PRESENCE_NAMESPACE, PRESENCE_VERSION)?;
        buf.segment_str(channel);
        Ok(buf.finish_prefix()?)
    }
}

impl Bus for KvBus {
    fn name(&self) -> &'static str {
        self.kv.store().name()
    }

    fn capabilities(&self) -> BusCapabilities {
        let store = self.kv.capabilities();
        BusCapabilities {
            cross_process: store.pubsub_cross_process,
            // No shipped store keeps a message after it has been delivered, so
            // `Last-Event-ID` resumption has to come from the application's own
            // log. Claiming otherwise would make a resumed SSE stream silently
            // lose events.
            replay: false,
            // A pattern subscription needs the backend's own `PSUBSCRIBE`;
            // nothing in `KvStore` exposes one, so this is honestly false and
            // `subscribe_pattern` falls through to `Error::Unsupported`.
            patterns: false,
            // Presence is keys with a TTL, which every store has, but
            // enumerating them needs `scan`.
            presence: store.scan,
            max_message_bytes: MAX_MESSAGE_BYTES,
        }
    }

    fn publish_raw<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.kv.publish(&self.qualified(channel), payload).await })
    }

    fn subscribe_raw<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move { self.kv.subscribe(&self.qualified(channel)).await })
    }

    fn presence(&self) -> Option<&dyn Presence> {
        // Only when the store can enumerate keys: a presence tracker that can
        // record a member and never list one is worse than none.
        self.kv.capabilities().scan.then_some(self)
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            match self.kv.health().await {
                moso_core::health::HealthStatus::Down(reason) => Err(crate::Error::Backend {
                    backend: self.name(),
                    operation: "probe",
                    source: reason.into(),
                }),
                _ => Ok(()),
            }
        })
    }
}

/// The largest message a `KvBus` will publish.
///
/// One mebibyte, which is what Redis' default `proto-max-bulk-len` and
/// PostgreSQL's `NOTIFY` payload limit both sit comfortably under. Refusing
/// here rather than at the backend means the error names the topic.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

impl Presence for KvBus {
    fn join<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // The member's name is the value as well as part of the key, so
            // `members` can report it without unescaping the key.
            let key = self.member_key(channel, member)?;
            self.kv
                .store()
                .set(
                    &key,
                    Bytes::from(member.to_owned().into_bytes()),
                    crate::SetOpts::new().ttl(ttl),
                )
                .await
                .map(|_| ())
        })
    }

    fn heartbeat<'a>(
        &'a self,
        channel: &'a str,
        member: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = self.member_key(channel, member)?;
            // `expire` on a key that has already gone reports false; re-joining
            // is what a client resuming after a pause expects, and it is one
            // round trip either way.
            if self.kv.store().expire(&key, ttl).await? {
                return Ok(());
            }
            self.join(channel, member, ttl).await
        })
    }

    fn leave<'a>(&'a self, channel: &'a str, member: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = self.member_key(channel, member)?;
            self.kv.store().delete(&key).await.map(|_| ())
        })
    }

    fn members<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let prefix = self.channel_prefix(channel)?;
            let mut cursor = crate::ScanCursor::start();
            let mut names = Vec::new();

            // There is no "leave" message to lose: a member is present while
            // its key is, and a crashed process expires on its own.
            loop {
                let (keys, next) = self.kv.store().scan(&prefix, cursor, 256).await?;
                for key in keys {
                    if let Some(value) = self.kv.store().get(&key).await?
                        && let Ok(name) = String::from_utf8(value.to_vec())
                    {
                        names.push(name);
                    }
                }
                if next.is_end() {
                    break;
                }
                cursor = next;
            }

            names.sort();
            names.dedup();
            Ok(names)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    /// What arrives in a user's notification feed.
    #[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Notification {
        /// What to show.
        text: String,
    }

    /// One user's notification feed.
    struct UserNotifications(u64);

    impl Topic for UserNotifications {
        type Message = Notification;
        const NAME: &'static str = "notifications";

        fn instance(&self) -> Cow<'_, str> {
            self.0.to_string().into()
        }
    }

    /// A topic with exactly one channel.
    struct Broadcast;

    impl Topic for Broadcast {
        type Message = Notification;
        const NAME: &'static str = "broadcast";

        fn instance(&self) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
    }

    /// The instance key is what makes one topic type serve every user, and a
    /// channel name that lost it would deliver every user's notifications to
    /// everyone.
    #[test]
    fn a_topics_channel_carries_its_instance() {
        assert_eq!(UserNotifications(7).channel(), "notifications:7");
        assert_eq!(UserNotifications(8).channel(), "notifications:8");
        // A topic with one channel does not get a trailing separator.
        assert_eq!(Broadcast.channel(), "broadcast");
    }

    /// The whole point of the typed layer: what goes in comes out.
    #[tokio::test]
    async fn a_typed_message_round_trips() {
        let bus = LocalBus::new();
        let topic = UserNotifications(7);
        let mut stream = bus.subscribe(&topic).await.expect("subscribes");

        bus.publish(
            &topic,
            &Notification {
                text: "your order shipped".to_owned(),
            },
        )
        .await
        .expect("publishes");

        assert_eq!(
            stream.next().await.expect("a message").text,
            "your order shipped",
        );
    }

    /// Two users must not see each other's notifications, which is the one
    /// thing the instance key exists to guarantee.
    #[tokio::test]
    async fn a_subscriber_hears_only_its_own_instance() {
        let bus = LocalBus::new();
        let mut seven = bus
            .subscribe(&UserNotifications(7))
            .await
            .expect("subscribes");
        let mut eight = bus
            .subscribe(&UserNotifications(8))
            .await
            .expect("subscribes");

        bus.publish(
            &UserNotifications(7),
            &Notification {
                text: "for seven".to_owned(),
            },
        )
        .await
        .expect("publishes");

        assert_eq!(seven.next().await.expect("a message").text, "for seven");
        // Nothing for eight: `now_or_never` rather than a timeout, because a
        // message that has not been published cannot arrive later either.
        assert!(futures_util::FutureExt::now_or_never(eight.next()).is_none());
    }

    /// Every subscriber to one channel gets every message, which is what makes
    /// this a bus rather than a queue.
    #[tokio::test]
    async fn every_subscriber_to_a_channel_receives_the_message() {
        let bus = LocalBus::new();
        let topic = UserNotifications(1);
        let mut a = bus.subscribe(&topic).await.expect("subscribes");
        let mut b = bus.subscribe(&topic).await.expect("subscribes");

        let delivered = bus
            .publish(
                &topic,
                &Notification {
                    text: "hello".to_owned(),
                },
            )
            .await
            .expect("publishes");
        assert_eq!(delivered, 2);

        assert_eq!(a.next().await.expect("a message").text, "hello");
        assert_eq!(b.next().await.expect("a message").text, "hello");
    }

    /// Publishing into an empty room is a no-op, not an error: a user with no
    /// socket open is the common case.
    #[tokio::test]
    async fn publishing_with_no_subscriber_is_not_an_error() {
        let bus = LocalBus::new();
        assert_eq!(
            bus.publish(
                &UserNotifications(99),
                &Notification {
                    text: "nobody is listening".to_owned(),
                },
            )
            .await
            .expect("publishes"),
            0,
        );
    }

    /// One bad publish from an older deploy must not end a subscriber's stream
    /// and disconnect every socket behind it.
    #[tokio::test]
    async fn a_message_that_does_not_decode_is_skipped_and_counted() {
        let bus = LocalBus::new();
        let topic = UserNotifications(3);
        let mut stream = bus.subscribe(&topic).await.expect("subscribes");

        // A shape an older deploy might have published.
        bus.publish_raw(&topic.channel(), Bytes::from_static(br#"{"nope":1}"#))
            .await
            .expect("publishes");
        bus.publish(
            &topic,
            &Notification {
                text: "the good one".to_owned(),
            },
        )
        .await
        .expect("publishes");

        assert_eq!(stream.next().await.expect("a message").text, "the good one");
        assert_eq!(stream.skipped(), 1, "the bad one was counted, not returned");
    }

    /// A message larger than the backend accepts is refused here, where the
    /// error can name the topic, rather than at the backend.
    #[tokio::test]
    async fn an_oversized_message_is_refused_with_the_channel_named() {
        struct Tiny;
        impl Bus for Tiny {
            fn name(&self) -> &'static str {
                "tiny"
            }
            fn capabilities(&self) -> BusCapabilities {
                BusCapabilities {
                    max_message_bytes: 4,
                    ..BusCapabilities::in_process()
                }
            }
            fn publish_raw<'a>(&'a self, _: &'a str, _: Bytes) -> BoxFuture<'a, Result<u64>> {
                Box::pin(async { Ok(0) })
            }
            fn subscribe_raw<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
                Box::pin(async { Ok(Box::pin(futures_util::stream::empty()) as MessageStream) })
            }
        }

        let error = Tiny
            .publish(
                &UserNotifications(1),
                &Notification {
                    text: "far too long for four bytes".to_owned(),
                },
            )
            .await
            .expect_err("too large");
        assert!(error.to_string().contains("notifications:1"), "{error}");
    }

    /// The typed API has to reach `dyn Bus`, or an application that injects one
    /// cannot use it.
    #[tokio::test]
    async fn the_typed_api_works_through_a_trait_object() {
        let bus: std::sync::Arc<dyn Bus> = std::sync::Arc::new(LocalBus::new());
        let topic = UserNotifications(5);
        let mut stream = bus.subscribe(&topic).await.expect("subscribes");

        bus.publish(
            &topic,
            &Notification {
                text: "through a vtable".to_owned(),
            },
        )
        .await
        .expect("publishes");

        assert_eq!(
            stream.next().await.expect("a message").text,
            "through a vtable",
        );
    }

    /// A test that passes against the in-process bus and fails on two pods is
    /// worse than a capability flag, so the flag has to be honest.
    #[test]
    fn the_local_bus_does_not_claim_to_cross_a_process_boundary() {
        let bus = LocalBus::new();
        assert!(!bus.capabilities().cross_process);
        assert!(!bus.capabilities().replay);
        assert!(bus.capabilities().patterns);
        assert!(bus.capabilities().presence);
    }

    /// A pattern subscription is what a moderation dashboard watching every
    /// room needs.
    #[tokio::test]
    async fn a_pattern_subscription_merges_every_matching_channel() {
        let bus = LocalBus::new();
        // The channels have to exist to be matched, which is what subscribing
        // to each of them first does.
        let _seven = bus
            .subscribe_raw("notifications:7")
            .await
            .expect("subscribes");
        let _eight = bus
            .subscribe_raw("notifications:8")
            .await
            .expect("subscribes");
        let _other = bus.subscribe_raw("audit:1").await.expect("subscribes");

        let mut merged = bus
            .subscribe_pattern("notifications:*")
            .await
            .expect("subscribes");

        bus.publish_raw("notifications:7", Bytes::from_static(b"a"))
            .await
            .expect("publishes");
        bus.publish_raw("notifications:8", Bytes::from_static(b"b"))
            .await
            .expect("publishes");
        bus.publish_raw("audit:1", Bytes::from_static(b"c"))
            .await
            .expect("publishes");

        let mut seen = vec![
            merged.next().await.expect("one"),
            merged.next().await.expect("two"),
        ];
        seen.sort();
        assert_eq!(
            seen,
            vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")]
        );
        assert!(futures_util::FutureExt::now_or_never(merged.next()).is_none());
    }

    /// The glob grammar decides which sockets a pattern reaches, so it is
    /// asserted rather than assumed.
    #[test]
    fn the_glob_grammar_is_the_documented_one() {
        assert!(glob_matches("notifications:*", "notifications:7"));
        assert!(glob_matches("notifications:*", "notifications:"));
        assert!(!glob_matches("notifications:*", "audit:7"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("a?c", "abc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(glob_matches("a*c*e", "abcde"));
        assert!(!glob_matches("a*c*e", "abcd"));
        assert!(glob_matches("exact", "exact"));
        assert!(!glob_matches("exact", "exactly"));
    }

    /// Presence is keys with a lease: a member is present while it keeps
    /// refreshing and disappears on its own when a process dies.
    #[tokio::test]
    async fn a_member_expires_without_a_heartbeat() {
        let bus = LocalBus::new();
        let presence = bus.presence().expect("the local bus tracks presence");

        presence
            .join("room:1", "ada", Duration::from_millis(40))
            .await
            .expect("joins");
        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["ada"]
        );
        assert_eq!(presence.count("room:1").await.expect("counts"), 1);

        // A heartbeat within the lease keeps it alive.
        tokio::time::sleep(Duration::from_millis(25)).await;
        presence
            .heartbeat("room:1", "ada", Duration::from_millis(40))
            .await
            .expect("heartbeats");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["ada"]
        );

        // Without one it disappears, with no "leave" message to lose.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(presence.members("room:1").await.expect("lists").is_empty());
        assert_eq!(presence.count("room:1").await.expect("counts"), 0);
    }

    /// Leaving is immediate rather than waiting out the lease, because a user
    /// who closed a tab should vanish from the list at once.
    #[tokio::test]
    async fn leaving_removes_a_member_at_once() {
        let bus = LocalBus::new();
        let presence = bus.presence().expect("tracks presence");

        presence
            .join("room:1", "ada", Duration::from_secs(60))
            .await
            .expect("joins");
        presence
            .join("room:1", "grace", Duration::from_secs(60))
            .await
            .expect("joins");
        presence.leave("room:1", "ada").await.expect("leaves");

        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["grace"]
        );
    }

    /// Two rooms are two rooms.
    #[tokio::test]
    async fn presence_is_scoped_to_its_channel() {
        let bus = LocalBus::new();
        let presence = bus.presence().expect("tracks presence");

        presence
            .join("room:1", "ada", Duration::from_secs(60))
            .await
            .expect("joins");
        presence
            .join("room:2", "grace", Duration::from_secs(60))
            .await
            .expect("joins");

        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["ada"]
        );
        assert_eq!(
            presence.members("room:2").await.expect("lists"),
            vec!["grace"]
        );
        assert!(presence.members("room:3").await.expect("lists").is_empty());
    }

    /// A store with no pubsub is refused at construction, so the failure is at
    /// boot and not at the first message.
    #[test]
    fn a_store_without_pubsub_is_refused_at_construction() {
        // The memory store *has* pubsub, so this is the positive half; the
        // negative half is asserted by the capability check itself, which is
        // the only thing `new` looks at.
        let kv = crate::Kv::in_memory("shop").expect("builds");
        assert!(kv.capabilities().pubsub);
        assert!(KvBus::new(kv).is_ok());
    }

    /// Two applications sharing one Redis must not hear each other, which is
    /// what the qualified channel name is for.
    #[test]
    fn the_channel_is_prefixed_with_the_application_name() {
        let shop = KvBus::new(crate::Kv::in_memory("shop").expect("builds")).expect("builds");
        let blog = KvBus::new(crate::Kv::in_memory("blog").expect("builds")).expect("builds");

        assert_eq!(
            shop.qualified("notifications:7"),
            "moso:v1:shop:notifications:7"
        );
        assert_ne!(
            shop.qualified("notifications:7"),
            blog.qualified("notifications:7"),
        );
    }

    /// A `KvBus` over the memory store is honest about being in-process, and
    /// the typed API still works through it.
    #[tokio::test]
    async fn a_kv_bus_delivers_and_reports_its_backends_reach() {
        let kv = crate::Kv::in_memory("shop").expect("builds");
        let bus = KvBus::new(kv).expect("builds");

        // The memory store's pubsub is real and process-local, and the bus says
        // exactly that rather than claiming to cross a boundary.
        assert!(!bus.capabilities().cross_process);
        assert_eq!(bus.name(), "memory");

        let topic = UserNotifications(11);
        let mut stream = bus.subscribe(&topic).await.expect("subscribes");
        bus.publish(
            &topic,
            &Notification {
                text: "over kv".to_owned(),
            },
        )
        .await
        .expect("publishes");

        assert_eq!(stream.next().await.expect("a message").text, "over kv");
        bus.probe().await.expect("the memory store is reachable");
    }

    /// Presence over KV is keys with a TTL, and the members have to come back
    /// with the names they joined under.
    #[tokio::test]
    async fn kv_presence_records_and_lists_members() {
        let kv = crate::Kv::in_memory("shop").expect("builds");
        let bus = KvBus::new(kv).expect("builds");
        let Some(presence) = bus.presence() else {
            // The memory store supports `scan`, so this is unreachable; the
            // branch exists because `presence()` is honestly optional.
            panic!("the memory store can enumerate keys");
        };

        presence
            .join("room:1", "ada", Duration::from_secs(60))
            .await
            .expect("joins");
        presence
            .join("room:1", "grace", Duration::from_secs(60))
            .await
            .expect("joins");
        presence
            .join("room:2", "alan", Duration::from_secs(60))
            .await
            .expect("joins");

        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["ada".to_owned(), "grace".to_owned()],
        );
        assert_eq!(presence.count("room:1").await.expect("counts"), 2);

        presence.leave("room:1", "ada").await.expect("leaves");
        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["grace"]
        );
        assert_eq!(
            presence.members("room:2").await.expect("lists"),
            vec!["alan"]
        );
    }

    /// A member name with a colon in it must not be able to look like another
    /// channel's member, which is what the key builder's escaping is for.
    #[tokio::test]
    async fn a_hostile_member_name_cannot_forge_another_channel() {
        let kv = crate::Kv::in_memory("shop").expect("builds");
        let bus = KvBus::new(kv).expect("builds");
        let presence = bus.presence().expect("tracks presence");

        presence
            .join("room:1", "ada", Duration::from_secs(60))
            .await
            .expect("joins");
        // A name that, unescaped, would land in `room:1`.
        presence
            .join("room", "1:mallory", Duration::from_secs(60))
            .await
            .expect("joins");

        assert_eq!(
            presence.members("room:1").await.expect("lists"),
            vec!["ada"]
        );
    }

    // ── the cross-instance acceptance criterion ───────────────────────────

    /// `docs/03-batteries/34-mail-storage-realtime.md`: *"a message published on
    /// instance A reaches a subscriber on instance B within 50 ms"*.
    ///
    /// Two independent `Kv`s over the same PostgreSQL — two `LISTEN` sessions,
    /// two connection pools, nothing shared in this process but the database —
    /// which is the closest a single-process test gets to two pods, and is the
    /// thing the in-process bus cannot do.
    ///
    /// Gated on `DATABASE_URL`: a machine without the test database skips with
    /// a message rather than failing.
    #[cfg(feature = "pg-kv")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_published_on_one_instance_reaches_another_within_fifty_milliseconds() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "SKIPPED: DATABASE_URL is not set, so cross-instance delivery was not measured. \
                 Start the test database and re-run to check the 50 ms criterion.",
            );
            return;
        };

        /// The budget the acceptance criterion names.
        const BUDGET: Duration = Duration::from_millis(50);

        // Two stores, each with its own pool — the "two instances".
        let build = || async {
            let store = crate::backend::PostgresStore::connect(
                &url,
                "moso_kv_bus_test",
                4,
                Duration::from_secs(10),
            )
            .await
            .expect("the test database is reachable");
            let kv = crate::Kv::builder("realtime")
                .store(store)
                .build()
                .expect("builds");
            KvBus::new(kv).expect("postgres has LISTEN/NOTIFY")
        };

        let instance_a = build().await;
        let instance_b = build().await;

        // The whole point of the capability flag: this one crosses.
        assert!(
            instance_a.capabilities().cross_process,
            "the postgres backend has to report that it crosses a process boundary",
        );

        // A channel unique to this run, so two runs in parallel do not see each
        // other's messages.
        let topic = UserNotifications(
            u64::from(std::process::id()) * 1_000
                + u64::from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.subsec_millis())
                        .unwrap_or_default(),
                ),
        );

        let mut stream = instance_b.subscribe(&topic).await.expect("subscribes");

        // `LISTEN` is asynchronous: give the session a moment to be registered
        // before publishing, or the first message races the subscription and
        // the measurement is of the race rather than of the delivery.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let sent = std::time::Instant::now();
        instance_a
            .publish(
                &topic,
                &Notification {
                    text: "across the boundary".to_owned(),
                },
            )
            .await
            .expect("publishes");

        let received = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a message arrived within the timeout")
            .expect("the stream did not end");
        let elapsed = sent.elapsed();

        assert_eq!(received.text, "across the boundary");
        eprintln!("cross-instance delivery took {elapsed:?} against a {BUDGET:?} budget");
        assert!(
            elapsed < BUDGET,
            "a message published on instance A took {elapsed:?} to reach instance B, against a \
             {BUDGET:?} budget",
        );
    }

    /// Presence across two instances: a member that joined on one is visible
    /// from the other, and expires on its own when the first stops
    /// heartbeating. There is no "leave" message to lose.
    #[cfg(feature = "pg-kv")]
    #[tokio::test(flavor = "multi_thread")]
    async fn presence_recorded_on_one_instance_is_visible_from_another() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "SKIPPED: DATABASE_URL is not set, so cross-instance presence was not checked."
            );
            return;
        };

        let build = || async {
            let store = crate::backend::PostgresStore::connect(
                &url,
                "moso_kv_bus_test",
                4,
                Duration::from_secs(10),
            )
            .await
            .expect("the test database is reachable");
            let kv = crate::Kv::builder("realtime")
                .store(store)
                .build()
                .expect("builds");
            KvBus::new(kv).expect("builds")
        };

        let instance_a = build().await;
        let instance_b = build().await;

        let room = format!("room:{}", std::process::id());
        let a = instance_a.presence().expect("postgres can enumerate keys");
        let b = instance_b.presence().expect("postgres can enumerate keys");

        a.join(&room, "ada", Duration::from_secs(30))
            .await
            .expect("joins");
        assert_eq!(
            b.members(&room).await.expect("lists"),
            vec!["ada".to_owned()],
            "a member that joined on A must be visible from B",
        );

        a.leave(&room, "ada").await.expect("leaves");
        assert!(
            b.members(&room).await.expect("lists").is_empty(),
            "a member that left on A must be gone from B",
        );
    }
}
