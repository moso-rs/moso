//! The readiness probe for a [`Kv`].
//!
//! Registered in the composition root:
//!
//! ```no_run
//! # use moso_core::HealthCheck as _;
//! # use moso_kv::Kv;
//! # fn example(kv: &Kv) {
//! // App::new(config).health_check("cache", kv.health_check())
//! let check = kv.health_check().non_critical();
//! assert!(!check.critical());
//! # let _ = check;
//! # }
//! ```
//!
//! # Critical, or not?
//!
//! **Not**, by default, and the reason is the whole failure policy in one
//! sentence: a cache whose namespaces all
//! [degrade](crate::FailureMode::Degrade) is a cache whose absence the
//! application survives, and taking every instance out of rotation over it
//! turns a degraded service into an outage. The report still says `down:
//! connection refused`, which is what an operator needs.
//!
//! An application whose sessions live in the same store declares
//! [`critical()`](KvHealthCheck::critical_check): losing sessions is not
//! degradation, it is logging everybody out.

use moso_core::app::Resolver;
use moso_core::{BoxFuture, HealthCheck, HealthStatus};

use crate::kv::Kv;

/// A [`HealthCheck`] over one [`Kv`] handle.
///
/// ```
/// use moso_core::{HealthCheck as _, HealthStatus};
/// use moso_kv::Kv;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let kv = Kv::in_memory("shop").expect("built");
/// let check = kv.health_check();
///
/// assert!(!check.critical(), "a degrading cache is not disqualifying");
/// assert_eq!(check.probe().await, HealthStatus::Up);
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KvHealthCheck {
    kv: Kv,
    critical: bool,
}

impl KvHealthCheck {
    /// A non-critical check over `kv`.
    ///
    /// ```
    /// use moso_core::HealthCheck as _;
    /// use moso_kv::{Kv, KvHealthCheck};
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert!(!KvHealthCheck::new(kv).critical());
    /// ```
    #[must_use]
    pub fn new(kv: Kv) -> Self {
        Self {
            kv,
            critical: false,
        }
    }

    /// Make failure disqualifying: the instance leaves rotation.
    ///
    /// For a store that holds sessions or locks rather than a cache.
    ///
    /// ```
    /// use moso_core::HealthCheck as _;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert!(kv.health_check().critical_check().critical());
    /// ```
    #[must_use]
    pub fn critical_check(mut self) -> Self {
        self.critical = true;
        self
    }

    /// Make failure non-disqualifying. The default; here so a configuration
    /// flag can go either way in one expression.
    ///
    /// ```
    /// use moso_core::HealthCheck as _;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert!(!kv.health_check().critical_check().non_critical().critical());
    /// ```
    #[must_use]
    pub fn non_critical(mut self) -> Self {
        self.critical = false;
        self
    }

    /// The handle this check probes.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.health_check().kv().app(), "shop");
    /// ```
    #[must_use]
    pub fn kv(&self) -> &Kv {
        &self.kv
    }

    /// Run the probe without a [`Resolver`].
    ///
    /// What [`HealthCheck::check`] delegates to, split out so a test — or a
    /// CLI command — can run it without an `AppState`.
    ///
    /// ```
    /// use moso_core::HealthStatus;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.health_check().probe().await, HealthStatus::Up);
    /// # }
    /// ```
    pub async fn probe(&self) -> HealthStatus {
        self.kv.health().await
    }
}

impl HealthCheck for KvHealthCheck {
    fn check<'a>(&'a self, _resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
        Box::pin(self.probe())
    }

    fn critical(&self) -> bool {
        self.critical
    }
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;

    fn kv() -> Kv {
        Kv::in_memory("shop").expect("built")
    }

    #[tokio::test]
    async fn a_reachable_store_is_up() {
        assert_eq!(kv().health_check().probe().await, HealthStatus::Up);
    }

    #[test]
    fn it_is_not_critical_by_default() {
        assert!(!kv().health_check().critical());
    }

    #[test]
    fn criticality_is_a_choice_in_both_directions() {
        let check = kv().health_check();
        assert!(check.clone().critical_check().critical());
        assert!(!check.critical_check().non_critical().critical());
    }

    #[test]
    fn it_holds_on_to_its_handle() {
        let check = kv().health_check();
        assert_eq!(check.kv().app(), "shop");
        assert!(format!("{check:?}").contains("KvHealthCheck"));
    }

    #[tokio::test]
    async fn it_is_usable_as_the_trait_object_the_app_stores() {
        let check: std::sync::Arc<dyn HealthCheck> = std::sync::Arc::new(kv().health_check());
        assert!(!check.critical());
    }
}
