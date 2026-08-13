//! Action markers: the `A` in [`Policy<A, R>`](crate::Policy).
//!
//! An action is a unit type. That is what lets `impl Policy<Edit, Post> for
//! Actor<Role>` exist in the application's crate without falling foul of the
//! orphan rule, and it is what makes `authorized_for::<Read>` a compile-time
//! selection rather than a string lookup.

/// A verb a policy can be asked about.
///
/// ```
/// use moso_authz::Action;
///
/// /// Publishing a draft.
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct Publish;
///
/// impl Action for Publish {
///     const NAME: &'static str = "publish";
/// }
///
/// assert_eq!(Publish::NAME, "publish");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an action",
    label = "not an action",
    note = "an action is a unit struct with a `NAME`, used as the `A` in `Policy<A, R>`",
    note = "help: declare a group of them at once — `moso_authz::actions! {{ Read, Edit, Publish }}`",
    note = "help: or write `#[derive(Clone, Copy, Debug, Default)] pub struct {Self};` and \
            `impl Action for {Self} {{ const NAME: &'static str = \"…\"; }}`"
)]
pub trait Action: Copy + Default + Send + Sync + 'static {
    /// The action's name, for explain traces and audit records.
    ///
    /// Lowercase, no spaces: `"publish"`, `"edit"`, `"read"`.
    const NAME: &'static str;
}

/// Which role registry answers questions about an action.
///
/// [`Authorized<A, R>`](crate::Authorized) has to resolve an actor before it can
/// run a policy, and it has three type parameters — the action, the resource and
/// the resource source — none of which is the application's role enum. A fourth
/// would make the documented `Authorized<Publish, Post>` unwritable. So the role
/// travels on the *action*, which is the one parameter every authorization
/// question already names.
///
/// An application declares it once per action, which
/// [`actions!`](crate::actions) does for a whole group:
///
/// ```
/// use moso_authz::{Action, HasRole};
/// # use moso_authz::{PermSet, Permission, perm::fingerprint_of};
/// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Perm { Read }
/// # impl Permission for Perm {
/// #     const ALL: &'static [Self] = &[Perm::Read];
/// #     const FINGERPRINT: u64 = fingerprint_of(&["posts.read"]);
/// #     fn index(self) -> u16 { 0 }
/// #     fn from_index(i: u16) -> Option<Self> { (i == 0).then_some(Perm::Read) }
/// #     fn as_str(self) -> &'static str { "posts.read" }
/// #     fn description(self) -> &'static str { "View posts" }
/// #     fn group(self) -> &'static str { "posts" }
/// #     fn parse(n: &str) -> Option<Self> { (n == "posts.read").then_some(Perm::Read) }
/// # }
/// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Role { Viewer }
/// # impl moso_authz::Role for Role {
/// #     type Perm = Perm;
/// #     const ALL: &'static [Self] = &[Role::Viewer];
/// #     fn index(self) -> u8 { 0 }
/// #     fn from_index(i: u8) -> Option<Self> { (i == 0).then_some(Role::Viewer) }
/// #     fn as_str(self) -> &'static str { "viewer" }
/// #     fn description(self) -> &'static str { "Read-only" }
/// #     fn permissions(self) -> PermSet<Perm> { PermSet::of([Perm::Read]) }
/// #     fn parse(n: &str) -> Option<Self> { (n == "viewer").then_some(Role::Viewer) }
/// # }
/// moso_authz::actions! {
///     for Role;
///     /// Making a draft public.
///     Publish = "publish",
/// }
///
/// assert_eq!(Publish::NAME, "publish");
/// fn takes_role<A: HasRole>() {}
/// takes_role::<Publish>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not say which roles answer for it",
    label = "no role registry for this action",
    note = "`Authorized<{Self}, _>` resolves an `Actor<Role>` before it runs a policy, and the \
            role type has to come from somewhere; it comes from the action",
    note = "help: declare the whole group at once — `moso_authz::actions! {{ for Role; {Self} = \
            \"…\" }}`",
    note = "help: or by hand — `impl HasRole for {Self} {{ type Role = Role; }}`"
)]
pub trait HasRole: Action {
    /// The application's role enum, from `moso::roles!`.
    type Role: crate::Role;
}

/// Declare action markers.
///
/// A `macro_rules!` and not a proc macro, because there is no case conversion
/// to do: the name is given.
///
/// The `for Role;` header is optional and declares [`HasRole`] for every action
/// in the group. Leave it out only for actions that are never used with
/// [`Authorized`](crate::Authorized) — the imperative
/// [`Actor::can`](crate::Actor::can) and
/// [`authorized_for`](crate::AuthorizedQuery::authorized_for) do not need it,
/// because the caller already holds the actor.
///
/// ```
/// use moso_authz::Action;
///
/// moso_authz::actions! {
///     /// Reading a resource.
///     Read = "read",
///     /// Changing a resource.
///     Edit = "edit",
///     /// Making a draft public.
///     Publish = "publish",
/// }
///
/// assert_eq!(Publish::NAME, "publish");
/// let _ = Read;
/// ```
#[macro_export]
macro_rules! actions {
    (for $role:ty; $( $(#[$meta:meta])* $name:ident = $wire:literal ),+ $(,)?) => {
        $crate::actions! { $( $(#[$meta])* $name = $wire ),+ }
        $(
            impl $crate::HasRole for $name {
                type Role = $role;
            }
        )+
    };
    ($( $(#[$meta:meta])* $name:ident = $wire:literal ),+ $(,)?) => {
        $(
            $(#[$meta])*
            #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug,
                     ::core::default::Default, ::core::cmp::PartialEq, ::core::cmp::Eq)]
            pub struct $name;

            impl $crate::Action for $name {
                const NAME: &'static str = $wire;
            }
        )+
    };
}

/// A name a path parameter is read under.
///
/// `Authorized<Publish, Post, FromPath<PostId>>` needs to carry a *string* in a
/// type position. Const generics of type `&'static str` are not stable, so the
/// string travels on a marker type's associated constant instead. Declare one
/// with [`path_name!`](crate::path_name).
///
/// ```
/// use moso_authz::PathName;
///
/// moso_authz::path_name!(
///     /// The `{post_id}` segment.
///     PostId = "post_id"
/// );
///
/// assert_eq!(PostId::NAME, "post_id");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not name a path parameter",
    label = "not a path-parameter name",
    note = "`FromPath<N>` needs `N` to carry the parameter's name as an associated constant, \
            because a `&'static str` cannot be a const generic argument on stable",
    note = "help: declare it — `moso_authz::path_name!(PostId = \"post_id\");` — then write \
            `Authorized<Publish, Post, FromPath<PostId>>`",
    note = "help: if the parameter is just `{{id}}`, drop the third parameter: \
            `Authorized<Publish, Post>` already reads it"
)]
pub trait PathName: Send + Sync + 'static {
    /// The path parameter's name, without braces.
    const NAME: &'static str;
}

/// Declare a [`PathName`] marker.
///
/// ```
/// use moso_authz::PathName;
///
/// moso_authz::path_name!(
///     /// The `{invoice_id}` segment.
///     InvoiceId = "invoice_id"
/// );
///
/// assert_eq!(InvoiceId::NAME, "invoice_id");
/// ```
#[macro_export]
macro_rules! path_name {
    ($( $(#[$meta:meta])* $name:ident = $param:literal );+ $(;)?) => {
        $(
            $(#[$meta])*
            #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug)]
            pub struct $name;

            impl $crate::PathName for $name {
                const NAME: &'static str = $param;
            }
        )+
    };
}
