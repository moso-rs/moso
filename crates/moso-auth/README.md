# moso-auth

Moso's authentication battery: sessions, passwords, JWT, API keys, OAuth2/OIDC, passkeys, TOTP and
the account-lifecycle flows.

Part of [Moso](https://github.com/lowsbarrel/moso). See `docs/03-batteries/30-auth.md` for the design.

```rust,ignore
#[endpoint]
async fn me(Depends(CurrentUser(user)): Depends<CurrentUser<User>>) -> Result<UserOut> {
    Ok(user.into())
}
```

## Status

**Implemented.** Sessions, passwords, JWT (with JWKS), API keys, OAuth2/OIDC, passkeys, TOTP/MFA
and the account-lifecycle flows all ship; the `routes` module mounts the account endpoints and the
`Bearer` bearer-token flow is wired end to end. No `todo!()` remains, and the public surface has a
running `tests/public_surface.rs` proving every signature composes.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
