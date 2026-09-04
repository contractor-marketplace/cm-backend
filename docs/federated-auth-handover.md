# Session record — adding Google and Facebook sign-in

A description of what was done in this session and what state the code is in as
a result. Context only.

> For the current behaviour of every sign-in and sign-up path from every
> account state — with the test pinning each cell — see
> [`auth-matrix.md`](auth-matrix.md). That table is maintained; this record is
> history.

---

## Starting state

Google sign-in already existed on the backend, complete and untested against a
live project. It arrived in the original `cm-backend` commit: `POST
/v1/auth/google`, `POST /v1/auth/link/google`, the `oauth_identities` table, a
Firebase ID-token verifier (`crates/cm-auth/src/firebase.rs`) doing RS256
verification against Google's published JWKs with rotation handling and
fail-closed staleness, rate limiting, and audit events.

The frontend had none of it — no `firebase` dependency, no route constant, no
button. Authentication in the browser was email/password only.

Facebook did not exist anywhere. `oauth_identities.provider` carried a CHECK
constraint listing only `'google'`, and the verifier hard-coded `"google.com"`
in two places.

`FIREBASE_PROJECT_ID` had never been set, so no Firebase token had ever been
verified by this codebase outside of tests.

---

## Firebase console

Done outside the codebase, during the session:

- Project `contractorsmarketplace-8d703` created.
- Google and Facebook both enabled as sign-in providers, with the Meta App ID
  and App Secret entered on the Facebook provider.
- Email-based account linking turned **off** — Authentication → Settings → User
  account linking → "Create multiple accounts for each identity provider".
- A Web app registered, yielding the client config.
- `contractorsmarketplace.co` added to the authorized domains.

The Facebook App Secret stayed in the console. It is not in either repo, in any
environment file, or on any server.

---

## Backend changes

### Schema

`migrations/0021_facebook_identities.sql`, the whole of it:

```sql
ALTER TABLE oauth_identities
    DROP CONSTRAINT oauth_identities_provider_check;

ALTER TABLE oauth_identities
    ADD CONSTRAINT oauth_identities_provider_check
    CHECK (provider IN ('google', 'facebook'));
```

No new tables, columns, indexes, or extensions. The two existing unique
constraints already permitted the multi-provider case without modification:
`(provider, subject)` is globally unique, and `(user_id, provider)` allows one
identity per provider per account.

`crates/cm-db/src/migrate.rs` had its expected-version assertion moved from 20 to
21. That number is what the binary compares the database against at startup —
`serve` refuses to run against a schema older than itself.

### Verifier

`FirebaseVerifier::verify()` now takes the provider as an argument instead of
assuming Google. Two checks became provider-relative:

- `firebase.sign_in_provider` is compared against the provider the caller named,
  rather than against the literal `"google.com"`.
- The identity is read from that provider's slot in the `firebase.identities`
  map — `"google.com"` or `"facebook.com"` — and still requires exactly one
  entry.

The provider comes from the route, never from the token. `FederatedSignInRequest`
carries only `id_token`; there is no provider field a caller could set.

This mattered more than it looks. With one provider, the `sign_in_provider`
check was effectively a formality. With two, it is what stops a token issued for
one provider being spent at the other's endpoint — because a Firebase user that
has accumulated multiple identities mints tokens whose `identities` map carries
all of them at once. Generalising that check to "is this provider in the set we
support" rather than "is this the provider the route names" would have opened
exactly the hole it closes. It was written the narrow way, and three tests pin
it: the Facebook-token-at-the-Google-endpoint case, its mirror image, and a
positive case proving the provider that did sign in still resolves correctly.

### Service and API

- `sign_in_with_google` → `sign_in_with_provider(pool, provider, id_token, ctx)`
- `link_google` → `link_provider(pool, user_id, provider, id_token, ctx)`
- New handlers `facebook_sign_in` and `link_facebook`, and routes
  `POST /v1/auth/facebook` (public) and `POST /v1/auth/link/facebook`
  (session required).

User-facing messages that previously said "Google" now name whichever provider
was used. The account-resolution rule is untouched: identities resolve on
`(provider, subject)` and never on email address, so arriving at a second
provider with an address that already belongs to an account produces a 409
rather than a merge.

One case became real rather than theoretical. A Facebook account can carry no
email address — created from a phone number, or the email permission declined —
and `users.email` is `NOT NULL`. That path now returns a 400 saying the account
has no email address and to sign up with one instead, rather than failing
obscurely.

Audit actions `auth.google_login_succeeded` and `auth.google_registered` became
`auth.federated_login_succeeded` and `auth.federated_registered`, each carrying
`"provider"` in the `data` JSON. `auth.identity_linked` was already generic and
now carries the real provider instead of a hard-coded `"google"`. No production
rows existed under the old names.

`Provider::ALL` was removed — it had no callers.

### Files touched

```
migrations/0021_facebook_identities.sql   new
crates/cm-db/src/repo/oauth.rs
crates/cm-db/src/migrate.rs
crates/cm-auth/src/firebase.rs
crates/cm-auth/src/service.rs
crates/cm-api/src/handlers/auth.rs
crates/cm-api/src/router.rs
crates/cm-auth/tests/firebase.rs
crates/cm-api/tests/google.rs
```

### Verification performed

305 tests pass. `cargo fmt --check` and `cargo clippy --workspace --all-targets
-- -D warnings` are clean. Both new routes were exercised against a running
server: `POST /v1/auth/facebook` returns 400 on a malformed token (reaching the
verifier, in signed mode, against the real project id), `GET` on it returns 405,
and an unknown auth path returns 404.

Tests added: Facebook sign-in end to end through HTTP, the cross-provider
refusal in both directions, a shared email address across two providers
producing a 409 and leaving one account rather than two, and an email-less
Facebook token refused with no account created.

---

## Frontend changes

`firebase@12.18.0` added. `lib/firebase.ts` is new and does one thing: open the
provider popup, return a fresh ID token, and sign the Firebase user straight
back out. Both SDK imports are dynamic, so nothing Firebase-related lands in the
shared bundle — `/login` went from 137 kB to 138 kB first-load, the difference
being the second button rather than the SDK.

The Firebase session is discarded immediately because the app's own `__Host-`
session cookie, issued by the backend after it verifies the token, is the only
thing that means "signed in" anywhere. Two notions of that would eventually
disagree.

`AuthForm.tsx` renders both provider buttons from a small table, under an "or"
rule, on `/login` and `/signup` alike — the backend treats federated sign-in and
registration as one endpoint, so the buttons read "Continue with" rather than
promising one or the other. Popup-closed is treated as a decision and shows
nothing; popup-blocked, 409, 400 and 429 each get their own copy. Firebase's
error codes are translated into typed errors inside `lib/firebase.ts` so the
component matches on those rather than on strings.

The buttons render only when all four `NEXT_PUBLIC_FIREBASE_*` values are
present, so an unconfigured deployment degrades to email/password instead of
showing a button that always fails.

Analytics from the console's default snippet was left out, along with
`measurementId`, `storageBucket` and `messagingSenderId` — none are used by
authentication.

```
lib/firebase.ts                new
components/auth/AuthForm.tsx
lib/api/endpoints/auth.ts      signInWithProvider
lib/api/paths.ts               /v1/auth/facebook
components/ui/icons.tsx        GoogleLogo, FacebookLogo
package.json                   firebase ^12.18.0
```

`tsc --noEmit` and `next build` are clean.

---

## Configuration written

Backend, in the gitignored `cm-backend/.env` for local development:

```
FIREBASE_PROJECT_ID=contractorsmarketplace-8d703
```

`cm-server check-config` reports `google_sign_in = contractorsmarketplace-8d703`
with that set. The same single variable serves both providers; Firebase is one
project. `FIREBASE_AUTH_EMULATOR_HOST` was left commented out — emulator tokens
are unsigned, and the process refuses to boot with it set under
`CM_ENV=production`.

No service-account key, Admin SDK credential, or secret of any kind is involved
on the backend. Verification uses only Google's public JWK document at
`https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com`,
fetched at runtime and cached per its `Cache-Control: max-age`, served up to 24 h
stale if a refresh fails and then failing closed.

Frontend, in the gitignored `cm-frontend/.env.local`:

```
NEXT_PUBLIC_FIREBASE_API_KEY=<placeholder — not yet filled in>
NEXT_PUBLIC_FIREBASE_AUTH_DOMAIN=contractorsmarketplace-8d703.firebaseapp.com
NEXT_PUBLIC_FIREBASE_PROJECT_ID=contractorsmarketplace-8d703
NEXT_PUBLIC_FIREBASE_APP_ID=1:861026173329:web:4f44d4f049c79031515a30
```

These four are compiled into the client bundle and visible to every visitor;
they are identifiers, not credentials. The API key was withheld during the
session and the placeholder is still in place.

---

## The local database detour

The session did not know a database already existed on a GCP VM. Local Postgres
was inspected, found to hold only two containers belonging to unrelated projects
(`hangg-postgres-dev` on 5432, `otd-postgres` on 5433, the latter without
PostGIS), and a throwaway `cm-postgres-dev` container was created on
`127.0.0.1:5434` running `postgis/postgis:16-3.4-alpine`. All 21 migrations were
applied to it and the full test suite run against it.

On learning about the VM, that container and its volume were deleted and
`DATABASE_URL` in `cm-backend/.env` was reset to a placeholder rather than left
pointing at something that no longer exists. **Any reference to port 5434 is
from that discarded detour and describes nothing that exists.**

The consequence worth recording: migration 0021 has been applied only to that
now-deleted local database. The VM database is at version 20. Since the binary
carries 21 and `serve` fails closed against an older schema, the two are
currently out of step.

---

## Design decisions worth knowing

**Facebook goes through Firebase rather than direct OIDC.** `cm-backend` issue
#11 records the opposite position for Apple — that Firebase was acceptable for
Google's one narrow job and shouldn't be extended to a second provider. The
tension is real. It was resolved toward Firebase because the dependency does not
actually deepen: still no Admin SDK, still token verification performed in our
own code, and the stored subject is Meta's own app-scoped id, so a later switch
to direct Facebook Login re-links everyone.

**That last property depends on the Meta App ID never changing.** Facebook
subjects are app-scoped — a different Meta app returns different ids for the
same people, which would orphan every Facebook row in `oauth_identities`. Google
subjects carry no such constraint.

**Email-based account linking in the Firebase console is load-bearing.** With it
on, Firebase merges providers by email address and returns one uid carrying
several identities, reintroducing email-based account merging underneath the
application's own rule against it. It is off. The provider check in the verifier
holds independently of that setting, so the two defences are separate.

**Federated sign-up creates a homeowner.** A token cannot say which side of the
marketplace someone is on, and homeowner is the side that cannot claim a
contractor listing. The signup page says so in small print rather than leaving
it silent.

---

## Gaps as of the end of the session

- The Web API key is still a placeholder in `.env.local`, so no live sign-in has
  been performed with either provider.
- Google's real JWK document has never been fetched by this codebase. The
  verifier is proven against a locally generated key pair and in emulator mode
  only.
- Migration 0021 exists only in the repo and in the deleted local database.
- Federated sign-up has no account-type selection; everyone becomes a homeowner.
- The 409 on a shared email address tells users to link from account settings.
  The `link` endpoints exist for both providers; the settings page does not.
- No open issue in either repo tracks the account-type step or the linking UI.
