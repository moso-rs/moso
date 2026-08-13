//! [`cached!`](macro@crate::cached) — an async function with a cache in front of it.
//!
//! ```
//! use moso_kv::{minutes, Kv, Result};
//!
//! moso_kv::namespace! {
//!     /// Cached user profiles.
//!     pub Profile: u64 => Option<String>, ttl = minutes(5);
//! }
//!
//! moso_kv::cached! {
//!     #[cached(namespace = Profile, key = id)]
//!     /// Load a profile, from the cache when it is there.
//!     pub async fn load_profile(kv: &Kv, id: u64) -> Result<Option<String>> {
//!         Ok(Some(format!("profile {id}")))
//!     }
//! }
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
//! let kv = Kv::in_memory("shop")?;
//!
//! assert_eq!(load_profile(&kv, 7).await?, Some("profile 7".to_owned()));
//! // Second call: served from the cache, `load_profile::uncached` not run.
//! assert_eq!(load_profile(&kv, 7).await?, Some("profile 7".to_owned()));
//!
//! // Invalidation is explicit, and lives next to the function it invalidates.
//! assert!(load_profile::invalidate(&kv, &7).await?);
//! # Ok(())
//! # }
//! ```
//!
//! # What it gives you
//!
//! * **Single-flight de-duplication.** A hundred concurrent calls with the same
//!   key run the body once. Proved in `tests/cache.rs` with an `AtomicUsize`
//!   and a hundred tasks, not by inspection.
//! * **Negative caching.** A namespace whose value is an `Option` caches its
//!   `None` under [`NEGATIVE_TTL`](crate::Namespace::NEGATIVE_TTL), which is
//!   the usual stampede source: a lookup for something that is not there is
//!   the one the cache never protects.
//! * **Explicit invalidation.** `name::invalidate(&kv, &key)`, generated
//!   alongside. There is deliberately **no** automatic entity-change
//!   invalidation: inferring which cache entries a write affects is unreliable
//!   and silently wrong, and the right place to invalidate is the service
//!   function that did the writing.
//! * **The uncached body, still callable.** `name::uncached(..)` is the
//!   function you wrote, so a test can exercise it without a `Kv` and a
//!   background job can bypass the cache on purpose.
//!
//! # Why a `macro_rules!` and not an attribute
//!
//! An attribute macro needs a proc-macro crate, and `moso-kv` is a runtime
//! crate. Wrapping the function in `cached! { … }` costs one line and one level
//! of indentation, and buys the whole feature with no second crate in the
//! dependency graph and no compile-time cost for anybody who does not use it.
//! Everything an attribute would have written is written here.
//!
//! # The rules
//!
//! 1. `#[cached(..)]` comes **first**, before the doc comments. It has to: a
//!    `#[cached]` after `#[doc]` is indistinguishable from any other attribute
//!    to a declarative macro.
//! 2. The function returns [`crate::Result<T>`] where `T` is the namespace's
//!    value type. An application error gets there through `?` — see
//!    [`From<moso_core::Error>`](crate::Error) — and comes back out with its
//!    status intact.
//! 3. `key` is an expression over the parameters, evaluated **before** the
//!    body's arguments are moved into the closure.
//! 4. `kv` names the parameter holding the handle. It defaults to `kv`.

/// Put a cache in front of an async function.
///
/// | Option | Default | Meaning |
/// | --- | --- | --- |
/// | `namespace = <ty>` | required | the [`Namespace`](crate::Namespace) to store under |
/// | `key = <expr>` | required | the key, as an expression over the parameters |
/// | `kv = <ident>` | `kv` | which parameter is the [`Kv`](crate::Kv) |
///
/// ```
/// use moso_kv::{minutes, Kv, Result};
///
/// /// Something a handler would hold.
/// #[derive(Clone)]
/// pub struct Db;
///
/// moso_kv::namespace! {
///     /// Cached order totals, by customer and currency.
///     pub Totals: (u64, String) => Option<u64>, ttl = minutes(2);
/// }
///
/// moso_kv::cached! {
///     #[cached(namespace = Totals, key = (customer, currency.clone()), kv = cache)]
///     /// Total spend, in minor units.
///     pub async fn total(cache: &Kv, db: &Db, customer: u64, currency: String) -> Result<Option<u64>> {
///         let _ = (db, &currency);
///         Ok(Some(customer * 100))
///     }
/// }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
/// let kv = Kv::in_memory("shop")?;
/// assert_eq!(total(&kv, &Db, 3, "eur".to_owned()).await?, Some(300));
///
/// // The generated module carries the escape hatches.
/// assert_eq!(total::uncached(&kv, &Db, 3, "eur".to_owned()).await?, Some(300));
/// assert!(total::invalidate(&kv, &(3, "eur".to_owned())).await?);
/// assert_eq!(
///     total::cache_key(&kv, &(3, "eur".to_owned()))?.as_str(),
///     "moso:v1:shop:totals:1:3:eur",
/// );
/// # Ok(())
/// # }
/// ```
///
/// An unknown option is one compile error naming the three that exist:
///
/// ```compile_fail
/// # use moso_kv::{Kv, Result};
/// # moso_kv::namespace! { /// ns
/// # pub Ns: u64 => u64; }
/// moso_kv::cached! {
///     #[cached(namespace = Ns, key = id, ttl = 5)]
///     /// Nope.
///     pub async fn f(kv: &Kv, id: u64) -> Result<u64> { Ok(id) }
/// }
/// ```
#[macro_export]
macro_rules! cached {
    (
        #[cached( $($options:tt)* )]
        $(#[$meta:meta])*
        $vis:vis async fn $name:ident (
            $first:ident : $first_ty:ty $(, $arg:ident : $arg_ty:ty)* $(,)?
        ) -> $ret:ty
        $body:block
    ) => {
        // The default handle is the *first parameter*, captured here rather
        // than written as the literal `kv` in this macro's own body. A default
        // written here would carry this macro's hygiene and resolve at the
        // definition site — which is how `kv` ends up meaning some unrelated
        // `fn kv()` in the caller's module rather than the caller's parameter.
        $crate::__cached_opts! {
            [$(#[$meta])*] [$vis $name]
            [$first : $first_ty $(, $arg : $arg_ty)*] [$ret] [$body]
            [$crate::cached::NamespaceIsRequired] [$crate::cached::key_is_required()] [$first]
            $($options)* ,
        }
    };
}

/// The placeholder a `#[cached]` with no `namespace` fails against.
///
/// It is a real type so that the error is "the trait bound
/// `NamespaceIsRequired: Namespace` is not satisfied" — which names the missing
/// option — rather than "cannot find type `__namespace_is_required`", which
/// names an identifier the user never wrote.
///
/// ```compile_fail
/// # use moso_kv::{Kv, Result};
/// moso_kv::cached! {
///     #[cached(key = id)]
///     /// No namespace.
///     pub async fn f(kv: &Kv, id: u64) -> Result<u64> { Ok(id) }
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct NamespaceIsRequired;

/// The placeholder a `#[cached]` with no `key` fails against.
///
/// # Panics
///
/// Never called: the expansion it appears in does not type-check, so the
/// failure is a compile error naming the missing option.
///
/// ```compile_fail
/// # use moso_kv::{Kv, Result};
/// # moso_kv::namespace! { /// ns
/// # pub Ns: u64 => u64; }
/// moso_kv::cached! {
///     #[cached(namespace = Ns)]
///     /// No key.
///     pub async fn f(kv: &Kv, id: u64) -> Result<u64> { Ok(id) }
/// }
/// ```
#[must_use]
pub fn key_is_required() -> NamespaceIsRequired {
    NamespaceIsRequired
}

/// Fold one `name = value` option into the accumulator, then emit.
#[doc(hidden)]
#[macro_export]
macro_rules! __cached_opts {
    // Nothing left: emit.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
        $(,)*
    ) => {
        $crate::__cached_emit! {
            [$($meta)*] [$vis $name] [$($arg : $arg_ty),*] [$ret] [$body]
            [$ns] [$key] [$kv]
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
        namespace = $new:ty, $($tail:tt)*
    ) => {
        $crate::__cached_opts! {
            [$($meta)*] [$vis $name] [$($arg : $arg_ty),*] [$ret] [$body]
            [$new] [$key] [$kv]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
        key = $new:expr, $($tail:tt)*
    ) => {
        $crate::__cached_opts! {
            [$($meta)*] [$vis $name] [$($arg : $arg_ty),*] [$ret] [$body]
            [$ns] [$new] [$kv]
            $($tail)*
        }
    };

    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
        kv = $new:ident, $($tail:tt)*
    ) => {
        $crate::__cached_opts! {
            [$($meta)*] [$vis $name] [$($arg : $arg_ty),*] [$ret] [$body]
            [$ns] [$key] [$new]
            $($tail)*
        }
    };

    // Anything else: one error naming the three options that exist.
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
        $bad:ident = $($tail:tt)*
    ) => {
        ::core::compile_error!(::core::concat!(
            "`",
            ::core::stringify!($bad),
            "` is not a `#[cached]` option. The options are `namespace`, `key` and `kv`. \
             A TTL belongs on the namespace: namespace! { pub Ns: K => V, ttl = minutes(5); }"
        ));
    };
}

/// Write the cached wrapper, and the module beside it.
#[doc(hidden)]
#[macro_export]
macro_rules! __cached_emit {
    (
        [$($meta:tt)*] [$vis:vis $name:ident] [$($arg:ident : $arg_ty:ty),*] [$ret:ty] [$body:block]
        [$ns:ty] [$key:expr] [$kv:ident]
    ) => {
        $($meta)*
        ///
        /// Cached by `moso_kv::cached!`. The uncached body, an invalidator and
        /// the key builder are in the module of the same name.
        $vis async fn $name ( $($arg : $arg_ty),* ) -> $ret {
            // Evaluated before the arguments move into the closure, so that a
            // key expression may borrow them.
            let __moso_kv_key = $key;
            let __moso_kv_handle: &$crate::Kv = $kv;
            __moso_kv_handle
                .get_or_insert_with::<$ns, _, _>(&__moso_kv_key, move || async move {
                    $name::uncached( $($arg),* ).await
                })
                .await
        }

        /// The uncached body, the invalidator and the key builder for the
        /// function of the same name.
        ///
        /// A module and a function may share a name — they live in different
        /// namespaces — which is what lets `load_profile::invalidate` sit
        /// beside `load_profile` with nothing to remember.
        $vis mod $name {
            #[allow(unused_imports)]
            use super::*;

            /// The function body, with no cache in front of it.
            ///
            /// What the cached wrapper calls on a miss. Public so that a test
            /// can exercise the real work without a store, and so that a
            /// background refresh can bypass the cache deliberately.
            pub async fn uncached( $($arg : $arg_ty),* ) -> $ret $body

            /// Drop this function's cached value for `key`.
            ///
            /// Invalidation is explicit on purpose: call this from the service
            /// function that writes, where what changed is known.
            ///
            /// # Errors
            ///
            /// A backend failure, subject to the namespace's
            /// `on_failure` mode.
            // Generated, not written: a caller who never invalidates has not
            // left dead code lying about, so this must not warn.
            #[allow(dead_code)]
            pub async fn invalidate(
                kv: &$crate::Kv,
                key: &<$ns as $crate::Namespace>::Key,
            ) -> $crate::Result<bool> {
                kv.delete::<$ns>(key).await
            }

            /// The key this function's value is stored under.
            ///
            /// # Errors
            ///
            /// [`moso_kv::Error::Key`](crate::Error::Key) when the key is over
            /// the length limit.
            #[allow(dead_code)]
            pub fn cache_key(
                kv: &$crate::Kv,
                key: &<$ns as $crate::Namespace>::Key,
            ) -> $crate::Result<$crate::Key> {
                kv.key::<$ns>(key)
            }

            /// The namespace this function caches into.
            #[allow(dead_code)]
            pub type Namespace = $ns;
        }
    };
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use crate::namespace::{minutes, seconds};
    use crate::{Kv, Namespace as _, Result};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    crate::namespace! {
        /// Cached user profiles.
        pub Profile: u64 => Option<String>, ttl = minutes(5), negative_ttl = seconds(30);

        /// Cached totals with a compound key.
        pub Totals: (u64, String) => u64, ttl = minutes(1);

        /// A namespace whose loader always fails.
        pub Failing: u64 => u64, ttl = minutes(1);
    }

    crate::cached! {
        #[cached(namespace = Profile, key = id)]
        /// Load a profile.
        ///
        /// The call counter is a parameter rather than a `static` so that every
        /// test owns its own: `cargo test` runs these concurrently in one
        /// process, and a shared counter would make each of them depend on
        /// which others happened to be running.
        pub async fn load_profile(
            kv: &Kv,
            calls: &'static AtomicUsize,
            id: u64,
        ) -> Result<Option<String>> {
            let _ = kv;
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(if id == 0 {
                None
            } else {
                Some(format!("profile {id}"))
            })
        }
    }

    crate::cached! {
        #[cached(namespace = Totals, key = (customer, currency.clone()), kv = cache)]
        /// Total spend, with a renamed handle parameter and a compound key.
        pub async fn total(cache: &Kv, customer: u64, currency: String) -> Result<u64> {
            let _ = (cache, &currency);
            Ok(customer * 100)
        }
    }

    crate::cached! {
        #[cached(key = id, namespace = Profile)]
        /// The options in the other order, to prove the muncher does not care.
        async fn reordered(kv: &Kv, id: u64) -> Result<Option<String>> {
            let _ = kv;
            Ok(Some(format!("r{id}")))
        }
    }

    crate::cached! {
        #[cached(namespace = Failing, key = id)]
        /// Always fails, with a 404.
        pub async fn missing(kv: &Kv, id: u64) -> Result<u64> {
            let _ = (kv, id);
            // `?` on a `moso_core::Result` converts, and the status survives
            // the round trip back out.
            Err(moso_core::Error::not_found("thing"))?
        }
    }

    fn kv() -> Kv {
        Kv::in_memory("shop").expect("built")
    }

    #[tokio::test]
    async fn the_second_call_is_a_hit() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();

        assert_eq!(
            load_profile(&kv, &CALLS, 7).await.expect("value"),
            Some("profile 7".to_owned())
        );
        assert_eq!(
            load_profile(&kv, &CALLS, 7).await.expect("value"),
            Some("profile 7".to_owned())
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hundred_concurrent_calls_run_the_body_once() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();

        let mut handles = Vec::new();
        for _ in 0..100 {
            let kv = kv.clone();
            handles.push(tokio::spawn(
                async move { load_profile(&kv, &CALLS, 42).await },
            ));
        }
        for handle in handles {
            assert_eq!(
                handle.await.expect("joined").expect("value"),
                Some("profile 42".to_owned())
            );
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_none_is_cached_too() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();

        for _ in 0..3 {
            assert_eq!(load_profile(&kv, &CALLS, 0).await.expect("value"), None);
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        // ... under the shorter negative ttl.
        let ttl = kv.ttl::<Profile>(&0).await.expect("ttl").expect("a ttl");
        assert!(ttl <= seconds(30), "{ttl:?}");
    }

    #[tokio::test]
    async fn invalidate_makes_the_next_call_recompute() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();

        load_profile(&kv, &CALLS, 1).await.expect("value");
        assert!(
            load_profile::invalidate(&kv, &1)
                .await
                .expect("invalidated")
        );
        load_profile(&kv, &CALLS, 1).await.expect("value");
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);

        // Invalidating something that is not cached is `false`, not an error.
        assert!(!load_profile::invalidate(&kv, &999).await.expect("no-op"));
    }

    #[tokio::test]
    async fn the_uncached_body_is_still_callable_and_does_not_cache() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();

        for _ in 0..3 {
            load_profile::uncached(&kv, &CALLS, 2).await.expect("value");
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 3);
        assert!(!kv.exists::<Profile>(&2).await.expect("exists"));
    }

    #[tokio::test]
    async fn the_generated_key_is_the_namespace_key() {
        let kv = kv();
        assert_eq!(
            load_profile::cache_key(&kv, &7).expect("short").as_str(),
            "moso:v1:shop:profile:1:7"
        );
        assert_eq!(
            <load_profile::Namespace as crate::Namespace>::PREFIX,
            Profile::PREFIX
        );
    }

    #[tokio::test]
    async fn a_renamed_handle_and_a_compound_key_work() {
        let kv = kv();
        assert_eq!(total(&kv, 3, "eur".to_owned()).await.expect("value"), 300);
        assert_eq!(
            total::cache_key(&kv, &(3, "eur".to_owned()))
                .expect("short")
                .as_str(),
            "moso:v1:shop:totals:1:3:eur"
        );
        assert!(
            total::invalidate(&kv, &(3, "eur".to_owned()))
                .await
                .expect("invalidated")
        );
    }

    #[tokio::test]
    async fn the_options_may_come_in_any_order() {
        let kv = kv();
        assert_eq!(
            reordered(&kv, 5).await.expect("value"),
            Some("r5".to_owned())
        );
    }

    #[tokio::test]
    async fn an_application_error_keeps_its_status_through_the_cache() {
        let kv = kv();
        let error = missing(&kv, 1).await.expect_err("fails");
        let http: moso_core::Error = error.into();
        assert_eq!(http.status(), http::StatusCode::NOT_FOUND);

        // ... and nothing was cached.
        assert!(!kv.exists::<Failing>(&1).await.expect("exists"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_one_arc_and_each_get_their_own_value() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let kv = kv();
        let seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let kv = kv.clone();
            let seen = Arc::clone(&seen);
            handles.push(tokio::spawn(async move {
                let mut value = load_profile(&kv, &CALLS, 11).await.expect("value");
                // Each caller owns its copy, so mutating one is safe.
                if let Some(text) = value.as_mut() {
                    text.push('!');
                }
                seen.fetch_add(1, Ordering::SeqCst);
                value
            }));
        }
        for handle in handles {
            assert_eq!(
                handle.await.expect("joined"),
                Some("profile 11!".to_owned())
            );
        }
        assert_eq!(seen.load(Ordering::SeqCst), 8);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        // The stored value is untouched by the callers' mutations.
        assert_eq!(
            kv.get::<Profile>(&11).await.expect("get"),
            Some(Some("profile 11".to_owned()))
        );
    }
}
