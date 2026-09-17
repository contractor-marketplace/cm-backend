# Authentication matrix, with the side of the marketplace

The companion to [auth-matrix.md](auth-matrix.md). That table crosses four
account states with six attempts and holds the *why* of each behaviour. This
one adds the dimension it leaves out — which side of the marketplace an
account is on, and which side an attempt asks for — and is written to be read
by whoever is writing the E2E specs. [auth-state-machine.md](auth-state-machine.md)
draws the same cells as a graph, adds the session, credential, email, status
and claim lifecycles, and holds the path to full browser coverage.

Seven starting states × nine attempts = 63 cells. No cell says "as above":
every row carries its own status code, resulting state, justification and
assertion target, because the whole point of the exercise is that a reader can
open one row and write one test from it.

Evidence is the code at `46356eb`. Cells marked **no test** are behaviours
executed by a code path a sibling cell pins, without a test of their own —
a smaller claim than "tested", labelled as such.

## What the endpoints actually are

| Attempt | Request |
|---|---|
| Sign in with email | `POST /v1/auth/login` `{email, password}` → 200 session, 202 challenge, or 401 |
| Finish a challenge | `POST /v1/auth/login/verify` `{challenge_id, code}` → 200 session + `__Host-cm_device` |
| Sign up with email | `POST /v1/auth/register` `{email, display_name, password, account_type}` → 202 challenge |
| Sign in / up with Google | `POST /v1/auth/google` `{id_token, account_type?, email?, display_name?}` → 200 session |
| Sign in / up with Facebook | `POST /v1/auth/facebook` — same body, provider fixed by the route |

Errors are `{"error":{"code":…,"message":…}}`; `code` is the stable thing to
branch on (`invalid_request` 400, `unauthenticated` 401, `forbidden` 403,
`conflict` 409, `too_many_requests` 429). Four messages recur often enough to
name once:

- **FED-400** — *"No account here yet uses that Google sign-in. Create an account first and choose whether you are a homeowner or a contractor — it cannot be changed later."* (`Facebook` substituted by route.)
- **FED-409** — *"An account already uses that email address. Sign in to that account instead."*
- **REG-409** — *"That email address is already registered."*
- **LOGIN-401** — body is the contentless *"Authentication is required."*; the UI renders its own *"That email and password do not match an account."*

## The four rules that generate the table

1. **Identity is `(provider, subject)`.** `oauth::find_by_subject` is the only
   lookup a token gets. An email on a token never finds an account.
2. **Email is a tripwire.** `users_email_norm_key` is unique over a generated
   normalised column; a colliding insert is a 409 and never a merge or an
   update. `NULL` collides with nothing.
3. **The side-check precedes the insert** (`service.rs:1522`, then `:1536`).
   A federated call that resolves no identity is refused for a missing
   `account_type` *before* the address is ever offered to the unique index.
   This is why every federated **sign-in** cell below is 400 and not 409, even
   when the address is taken — and it is the single most counter-intuitive
   thing in this document.
4. **`account_type` describes a new account only.** A resolved identity skips
   the create branch entirely, so the field is not read, not compared, and not
   stored. There is no endpoint anywhere that writes `users.account_type`
   after insert.

## What the two pages send, and what that hides

Both pages render the same component ([AuthForm.tsx](../../cm-frontend/components/auth/AuthForm.tsx)).
`/login` sends no `account_type` from either the form or the provider buttons.
`/signup` shows a two-radio fieldset (`name="account_type"`, values
`homeowner` / `contractor`) and **refuses to open the provider popup** until
one is chosen — `onProvider` returns early with *"Choose whether you are a
homeowner or a contractor."* (`AuthForm.tsx:191`).

Consequence for the suite: **FED-400 is unreachable from `/signup`**. A
Playwright spec that wants it must use `/login`'s provider button, or call the
API directly. A spec that wants the client-side guard must click the provider
button on `/signup` with no radio selected and assert no network call leaves
the page.

## Where the role is observable

`window.location.assign(afterSignIn(next))` sends both roles to **`/app`**.
There is no `/homeowner/dashboard` and no `/contractor/dashboard`; role-based
routing does not exist in this product. Three things are assertable instead,
weakest to strongest:

| Probe | Homeowner | Contractor |
|---|---|---|
| Session response body | `user.account_type == "homeowner"` | `"contractor"` |
| `/app` second panel ([page.tsx:47](../../cm-frontend/app/\(app\)/app/page.tsx#L47)) | label *"Hiring someone"*, button *"Browse contractors"* | label *"Claim your listing"* (or *"Your listing"* once a claim is approved), button *"Find your listing"* |
| Capability gate | `POST /v1/jobs` → 201 · `POST /v1/contractors/{id}/claims` → **403** | `POST /v1/jobs` → **403** · claims → 201 |

Prefer the capability gate for anything asserting *role immutability*. It is
enforced in the handler (`jobs.rs:103`, `claims.rs:35`) and again by a database
trigger, so it cannot drift from the stored column the way copy can.

## Fixture conventions for the whole table

- **E** is one address, used by every attempt in every row. The matrix asks the
  hostile question — same human, same address, different door — so unless a
  row says otherwise, a federated attempt carries a **fresh provider subject**
  and the address **E**.
- Where a row's provider matches the starting state's provider, the token
  carries **that state's own subject**, because that is the only way the
  returning-identity path is reached.
- Where a cell's answer turns on the address, the *different-address* fork is
  named in the **Why** column. It is always the same fork: no collision, so a
  second, separate account is created, by design.
- Browsers are **unremembered** (no `__Host-cm_device`) unless stated. A
  remembered browser turns 202 into 200 on the password path and nothing else.
- Federated cells need minted tokens. The backend tests use the Firebase auth
  emulator (`emulator_router`, `google.rs:17`); the browser suite has no
  provider stub today, so these cells are API-level until `providerIdToken`
  ([lib/firebase.ts:117](../../cm-frontend/lib/firebase.ts#L117)) is made
  injectable. That is the single biggest blocker to E2E coverage here.

---

## 1 · No existing account

The database holds nothing. E is free; no provider subject resolves.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | **401** `unauthenticated`. A decoy Argon2 hash is verified so the timing matches a wrong password; audited `unknown_account`. | No session, no account, no cookies. | `login_precheck` → `Precheck::NoAccount`. An unknown address must be indistinguishable from a wrong password or the form becomes an address oracle. | Banner `[role="alert"]` reads *"That email and password do not match an account."*; no `__Host-cm_session` cookie; still on `/login`. Pinned by `every_login_failure_looks_the_same` (auth.rs:186). |
| Sign in with Google | **400** `invalid_request`, **FED-400**. | Nothing created — the test asserts `count(*) FROM users = 0`. No session. | Rule 3: identity miss → create branch → `intended_account_type` is `None` → refused rather than assigned a side that can never be changed. | Banner contains *"No account here yet uses that Google sign-in"* **and** *"cannot be changed"*; assert zero users created; no redirect to `/app`. Pinned by `federated_sign_in_without_an_account_refuses_rather_than_guessing` (google.rs:575). |
| Sign in with Facebook | **400** `invalid_request`, **FED-400** with *"Facebook"*. | Nothing created. No session. | Same function, provider fixed by the route (`facebook_sign_in`, auth.rs:311). | Same as the Google row, asserting the word *"Facebook"* in the banner. **No test** — the wording substitution is pinned only on the Google side. |
| Sign up as Homeowner with email | **202** `{challenge_id, email}`. The account row exists already, `account_type='homeowner'`, `status='active'`, `email_verified_at` NULL. | No session yet. Role fixed permanently at this insert. | `register` inserts user + password + code + outbox row in one transaction; the emailed code is the last step of registration, not a step after it. | Code screen appears (`CodeStep`); enter the mailbox code → 200, redirect to `/app`, `user.account_type == "homeowner"`, and `POST /v1/contractors/{id}/claims` → **403**. Pinned by `registering_returns_a_challenge_and_no_session` and `the_code_creates_a_session_and_verifies_the_address` (login_codes.rs:35, 66). |
| Sign up as Contractor with email | **202** `{challenge_id, email}`. Row created with `account_type='contractor'`. | No session yet. Role fixed permanently. | Identical path; `AccountType::parse_request` accepted `"contractor"` in the handler before the service ran. | After the code: `/app` shows *"Claim your listing"* and *"Find your listing"*; `POST /v1/jobs` → **403**. **No test** at the API level for the contractor side of registration; the helper `register_contractor` (common/mod.rs:352) exercises it as a fixture for other suites. |
| Sign up as Homeowner with Google | **200** session. Account created `homeowner`; email from the token's claim, else its `identities` slot, else the popup's copy (stored **unverified**), else NULL. | Session + `__Host-cm_session` + `__Host-cm_csrf`; one `oauth_identities` row. | Create branch with a side supplied. The address chain is most-proved-first because the required Firebase console mode strips the email claim from OAuth tokens. | Session body `user.account_type == "homeowner"`; redirect `/app`; claims endpoint → 403. Pinned by `federated_sign_up_creates_the_side_the_person_chose` (google.rs:543) and `a_first_google_sign_in_creates_an_account_and_a_session` (:120). |
| Sign up as Contractor with Google | **200** session. Account created `contractor`. | Session issued; role permanent. | Same branch, other side. The test counts contractor rows to prove the side was not defaulted. | `user.account_type == "contractor"`; `POST /v1/jobs` → **403**. Pinned by `federated_sign_up_creates_the_side_the_person_chose` (google.rs:543), whose second iteration asserts exactly one contractor row exists. |
| Sign up as Homeowner with Facebook | **200** session. Account created `homeowner`. If Facebook shared no address, the row is created with `email IS NULL` and display name *"Facebook user"*. | Session issued; role permanent; account may be address-less. | Rule 2: NULL collides with nothing, so an address-less provider account is creatable (0035) rather than turned away. | `user.account_type == "homeowner"` and `user.email` either E or `null`; the account page nudges an address-less account to add one. Pinned by `a_first_facebook_sign_in_creates_an_account_and_a_session` (google.rs:357), `a_facebook_account_without_an_email_still_gets_in` (:477). |
| Sign up as Contractor with Facebook | **200** session. Account created `contractor`. | Session issued; role permanent. | Same function as the Google contractor cell, provider fixed by the route. | `user.account_type == "contractor"`; `POST /v1/jobs` → **403**. **No test** — no case asserts the contractor side on the Facebook route; a per-provider fork in the side-handling would not be caught. |

## 2 · Existing Homeowner, password account on E

One user row: `account_type='homeowner'`, a password credential, address E.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | Correct password, unremembered browser → **202** challenge; remembered browser → **200** session. Wrong password → 401; eight failures lock the account even against the right password. | Session as **homeowner** after the code. Role untouched. | The password path never reads or writes `account_type`; it authenticates a user id. | Code screen, then `/app` with *"Hiring someone"* and `POST /v1/jobs` → 201. Pinned by `login_issues_a_new_session_distinct_from_the_first` (auth.rs:161), `a_remembered_browser_logs_in_without_a_code` (login_codes.rs:102), `eight_failures_lock_the_account_against_the_correct_password` (auth.rs:232). |
| Sign in with Google | **400** `invalid_request`, **FED-400** — *not* 409. | Nothing created, no session, existing homeowner untouched. | Rules 1 and 3 together: the Google subject resolves nothing, and the missing `account_type` is refused before the address E is offered to the unique index. The tripwire never fires. | Banner *"No account here yet uses that Google sign-in"*; assert `count(*) FROM users = 1`; then sign in with the password and assert `GET /v1/me` still reports `"homeowner"`. Non-matching pinned by `a_google_account_is_never_matched_to_an_existing_account_by_email` (google.rs:182); the 400-beats-409 ordering by `a_federated_sign_in_against_a_taken_address_is_refused_for_the_missing_side`. |
| Sign in with Facebook | **400** `invalid_request`, **FED-400** with *"Facebook"*. | Nothing created, no session. | Same ordering, other provider. | As above with *"Facebook"*. **No test**. |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409**. | No new account. Existing homeowner unchanged, still holding E. | Rule 2 at `users::insert` (users.rs:215). Nothing leaks beyond "this address is taken", which the person asserting it already knows. | Field error under the email input reading *"That email address is already registered."* (the client routes `conflict` to the email field, `AuthForm.tsx:155`); assert one users row. Pinned by `a_duplicate_address_is_refused` (auth.rs:91). |
| Sign up as Contractor with email | **409** `conflict`, **REG-409** — the same message, and the same 409. | **No new account, and the existing account is still a homeowner.** The `contractor` choice is parsed, accepted, and then discarded with the failed insert. | `register` only ever INSERTs; there is no UPDATE path that could apply the requested side. The address loses to the unique index first. Note the ordering *within* register: an empty display name, a malformed address or a weak password is a 400 **before** the 409 — a duplicate address plus a short password reports the password. | Assert 409, then sign in with the original password and assert `GET /v1/me` still `"homeowner"` and `POST /v1/contractors/{id}/claims` → 403. Pinned by `a_cross_side_duplicate_leaves_the_existing_account_on_its_side` (auth.rs). |
| Sign up as Homeowner with Google | **409** `conflict`, **FED-409**. | No new account, no `oauth_identities` row, no session. Existing homeowner untouched. | Create branch reached with a side supplied, so the insert is attempted and loses to `users_email_norm_key`; the error is rewritten method-agnostically because the colliding account may itself have no password. | Banner shows **FED-409** verbatim (the provider path renders `cause.displayMessage`, `AuthForm.tsx:225`); assert one users row and zero identity rows. Pinned by `a_google_account_is_never_matched_to_an_existing_account_by_email` (google.rs:182) and `a_client_address_colliding_with_an_existing_account_is_refused` (:752). |
| Sign up as Contractor with Google | **409** `conflict`, **FED-409**. | No new account; existing account still `homeowner`. The requested contractor side never materialises. | The **method/role clash**: `account_type='contractor'` is valid and is carried all the way to the insert, where the shared address kills the row before the side means anything. Rule 2 outranks the requested role. With a *different* address, this cell instead returns 200 and creates a genuinely separate contractor account — two accounts, one human, accepted by design. | Banner **FED-409**; assert `count(*) FROM users = 1` and `account_type = 'homeowner'`; assert no contractor capability anywhere (`POST /v1/contractors/{id}/claims` as the original account → 403). Insert-conflict path pinned by google.rs:752; **no test** carries `account_type: "contractor"` into a colliding insert. |
| Sign up as Homeowner with Facebook | **409** `conflict`, **FED-409**. | No new account, no session. | Same insert path; the provider only decides which identity row *would* have been written. | Banner **FED-409**; one users row. Cross-provider collision pinned by `a_shared_email_across_providers_is_a_conflict_not_a_merge` (google.rs:426); the password-account variant is the shared insert path. |
| Sign up as Contractor with Facebook | **409** `conflict`, **FED-409**. | No new account; existing account still `homeowner`. | As the Google contractor cell. Different address → separate contractor account, by design. | Banner **FED-409**; assert the existing row's `account_type` is unchanged. **No test**. |

## 3 · Existing Contractor, password account on E

One user row: `account_type='contractor'`, a password credential, address E.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | Correct password, unremembered browser → **202** challenge; remembered → **200** session. Eight failures lock. | Session as **contractor** after the code. Role untouched. | The password path authenticates a user id and never touches `account_type`. | Code screen, then `/app` showing *"Claim your listing"* / *"Find your listing"*, and `POST /v1/jobs` → **403**. Login shape pinned by auth.rs:161 and login_codes.rs:102; the contractor-side assertion is **untested** at this layer. |
| Sign in with Google | **400** `invalid_request`, **FED-400**. | Nothing created, no session, existing contractor untouched. | Rules 1 and 3: subject miss, then the missing side refused before the address is tested. | Banner *"No account here yet uses that Google sign-in"*; one users row; after a password sign-in, `GET /v1/me` still `"contractor"`. Non-matching pinned by google.rs:182; ordering **untested**. |
| Sign in with Facebook | **400** `invalid_request`, **FED-400** with *"Facebook"*. | Nothing created, no session. | Same path, other provider. | As above with *"Facebook"*. **No test**. |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409**. | **No new account, and the existing account is still a contractor.** | The mirror of §2's cross-role cell: INSERT-only registration, address loses to the unique index, requested homeowner side discarded. | Assert 409, then password sign-in and assert `GET /v1/me` is `"contractor"` and `POST /v1/jobs` → 403. Conflict pinned by `a_duplicate_address_is_refused` (auth.rs:91); the role-preservation assertion is **untested**. |
| Sign up as Contractor with email | **409** `conflict`, **REG-409**. | No new account; existing contractor unchanged. | Rule 2. The side matching makes no difference — a duplicate address is refused whether or not the requested side agrees with the stored one. | Field error under the email input with **REG-409**; one users row. Pinned by `a_duplicate_address_is_refused` (auth.rs:91). |
| Sign up as Homeowner with Google | **409** `conflict`, **FED-409**. | No new account; existing account still `contractor`. | Create branch, side supplied, insert loses to the address. Different address → a separate homeowner account, by design. | Banner **FED-409**; assert one users row with `account_type='contractor'`. Insert-conflict path pinned by google.rs:752; role preservation **untested**. |
| Sign up as Contractor with Google | **409** `conflict`, **FED-409**. | No new account, no identity row, no session. | Rule 2 again — matching sides do not license a merge. There is no route by which a Google identity attaches itself to a password account except `link_provider`, signed in. | Banner **FED-409**; then sign in with the password, `POST /v1/auth/link/google` with the same token → 204, and `GET /v1/me` shows `connected_providers` containing `google` **and** `account_type` still `"contractor"`. Linking pinned by `linking_requires_being_signed_in_and_is_one_per_provider` (google.rs:247) and `a_password_account_can_still_use_its_password_after_linking` (:338). |
| Sign up as Homeowner with Facebook | **409** `conflict`, **FED-409**. | No new account; existing contractor unchanged. | Shared insert path; provider decides only which identity row would have followed. | Banner **FED-409**; one users row, role unchanged. Cross-provider conflict pinned by google.rs:426. |
| Sign up as Contractor with Facebook | **409** `conflict`, **FED-409**. | No new account; existing contractor unchanged. | As above. | Banner **FED-409**; assert the stored role is untouched. **No test**. |

## 4 · Existing Homeowner, Google account (subject G, address E)

One user row, `account_type='homeowner'`, no password credential, one
`oauth_identities` row for Google subject G.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | **401** `unauthenticated`. Decoy hash burned; audited `no_password_set`. | No session. Account unchanged. | `Precheck::NoPassword`. Not a dead end: password reset **grants a federated account its first password** when an address is on file, after which both doors work. | Banner *"That email and password do not match an account."*; then `POST /v1/auth/password-reset/request` for E, follow the emailed link, and assert the password now signs in as `"homeowner"`. Decoy shape pinned by `every_login_failure_looks_the_same` (auth.rs:186); the escape hatch by `a_reset_gives_a_federated_account_its_first_password` (google.rs:863). |
| Sign in with Google (subject G) | **200** session. | Session as **homeowner**; `oauth_identities.last_login_at` touched. | Rule 1: same `(provider, subject)` → same account, every time. No `account_type` was sent and none was needed. | Redirect `/app`; `user.account_type == "homeowner"`; a second sign-in returns the same `user.id`. Pinned by `a_returning_google_user_gets_the_same_account` (google.rs:149). |
| Sign in with Facebook | **400** `invalid_request`, **FED-400** with *"Facebook"*. | Nothing created, no session, Google account untouched. | No Facebook identity on file, and the sign-in page sends no side. The sanctioned route is: sign in with Google, then **Connect Facebook** on the dashboard. | Banner names Facebook; then sign in with Google, `POST /v1/auth/link/facebook` → 204, and assert `GET /v1/me` lists both providers with `account_type` still `"homeowner"`. Dashboard's view pinned by `the_account_page_knows_which_providers_are_connected` (google.rs:826); the refusal wording is **untested** on Facebook. |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409**. | No new account; the Google account is unchanged and still has no password. | Rule 2 — the address on the federated row is as unique as any other. **Fork:** if this Google account was created address-less (possible since 0035), E is free and registration **succeeds with 202**, creating a second, separate homeowner account. | Assert 409 for the address-on-file fixture; assert 202 + two distinct `user.id`s for the address-less fixture. Same-address case is the shared `users::insert` conflict path (pinned from the email side by auth.rs:91); the **federated-address variant has no dedicated test**. |
| Sign up as Contractor with email | **409** `conflict`, **REG-409**. | No new account; the Google account is still a **homeowner**. | INSERT-only registration; the requested contractor side dies with the failed insert. Address-less fork as above, and there it creates a separate *contractor* account. | Assert 409, then sign in with Google and assert `GET /v1/me` is `"homeowner"`. **No test**. |
| Sign up as Homeowner with Google (subject G) | **200** session. | Session as **homeowner**. Nothing written but the identity's login timestamp. | Rule 4: the identity resolves, so the create branch — and with it `account_type` — is never reached. The field happens to agree here, which is exactly why it is the wrong cell to test immutability with. | `user.account_type == "homeowner"`; assert `count(*) FROM users` unchanged. Returning-identity path pinned by google.rs:149. |
| Sign up as Contractor with Google (subject G) | **200** session — **not an error**. | Session as **homeowner**. The `contractor` field is discarded, unread. Role unchanged in the database. | **Cross-role registration.** One endpoint serves both pages, and a resolved identity skips the create branch; `account_type` cannot re-side an existing account by any route. The 200 is deliberate: this is the sign-in → *"create an account first"* → sign-up funnel, and it should land the person in their account rather than in a second dead end. | Send `account_type: "contractor"`, assert **200** and `user.account_type == "homeowner"`; assert `SELECT account_type FROM users` is still `homeowner`; assert `/app` renders the homeowner panel and `POST /v1/contractors/{id}/claims` → **403**. Pinned by `the_account_type_field_cannot_re_type_an_existing_account` (google.rs:614). |
| Sign up as Homeowner with Facebook (new subject) | **409** `conflict`, **FED-409**. | No new account, no Facebook identity row, no session. | A genuinely new identity whose address collides with the Google account's. Rule 2 refuses it; the sanctioned merge is linking, signed in. **Fork:** a different or absent address creates a separate account instead. | Banner **FED-409**; assert one users row and one identity row. Pinned by `a_shared_email_across_providers_is_a_conflict_not_a_merge` (google.rs:426). |
| Sign up as Contractor with Facebook (new subject) | **409** `conflict`, **FED-409**. | No new account; the Google account is still a homeowner. | As above; the requested side is irrelevant once the address collides. | Banner **FED-409**; assert the Google account's role is untouched. Cross-provider conflict pinned by google.rs:426; the contractor-side variant is **untested**. |

## 5 · Existing Contractor, Google account (subject G, address E)

One user row, `account_type='contractor'`, no password credential, one Google
identity for subject G.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | **401** `unauthenticated`. Decoy hash; audited `no_password_set`. | No session. Account unchanged. | `Precheck::NoPassword`; reset grants a first password when an address is on file. | Generic banner; then reset → password signs in as `"contractor"`, and `POST /v1/jobs` → 403. Reset-for-federated pinned by google.rs:863 (on a homeowner fixture); the contractor variant is **untested**. |
| Sign in with Google (subject G) | **200** session. | Session as **contractor**; login timestamp touched. | Rule 1. | `user.account_type == "contractor"`; same `user.id` across repeat sign-ins; `POST /v1/jobs` → **403**. Returning-identity path pinned by google.rs:149 (homeowner fixture). |
| Sign in with Facebook | **400** `invalid_request`, **FED-400** with *"Facebook"*. | Nothing created, no session. | No Facebook identity on file; the sign-in page sends no side. Route to having both is Connect Facebook from the dashboard. | Banner names Facebook; then Google sign-in + `POST /v1/auth/link/facebook` → 204 and role still `"contractor"`. **No test**. |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409**. | No new account; the Google account is still a **contractor**. | Rule 2 first, requested side discarded. Address-less fork → 202 and a second, separate homeowner account. | Assert 409, then Google sign-in and assert `GET /v1/me` is `"contractor"`. **No test**. |
| Sign up as Contractor with email | **409** `conflict`, **REG-409**. | No new account; account unchanged, still without a password. | Rule 2; a matching side changes nothing. | Field error with **REG-409**; one users row. Shared insert path (pinned from the email side by auth.rs:91). |
| Sign up as Homeowner with Google (subject G) | **200** session — **not an error**. | Session as **contractor**. The `homeowner` field is discarded, unread. | **Cross-role registration, the direction the suite does not cover.** Identity resolves → create branch skipped → Rule 4. | Send `account_type: "homeowner"`, assert 200 and `user.account_type == "contractor"`; assert `POST /v1/jobs` → **403** (the strongest available proof the side did not flip); assert the stored column unchanged. Pinned by `the_account_type_field_cannot_re_type_an_existing_account`, which now loops both directions. |
| Sign up as Contractor with Google (subject G) | **200** session. | Session as **contractor**; nothing written but the login timestamp. | Rule 4; the field agrees and is still not read. | `user.account_type == "contractor"`; users count unchanged. Returning-identity path pinned by google.rs:149. |
| Sign up as Homeowner with Facebook (new subject) | **409** `conflict`, **FED-409**. | No new account; the contractor account is untouched. | New identity, colliding address, Rule 2. Different/absent address → a separate homeowner account. | Banner **FED-409**; one users row, role still `"contractor"`. Cross-provider conflict pinned by google.rs:426. |
| Sign up as Contractor with Facebook (new subject) | **409** `conflict`, **FED-409**. | No new account; contractor account unchanged. | As above. | Banner **FED-409**; assert one users row and one identity row. **No test**. |

## 6 · Existing Homeowner, Facebook account (subject F, address E)

One user row, `account_type='homeowner'`, no password credential, one Facebook
identity for subject F. The address may legitimately be NULL; where that
changes the answer it is called out.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | **401** `unauthenticated`. Decoy hash; audited `no_password_set`. | No session. Account unchanged. | `Precheck::NoPassword`. With an address on file, reset grants a first password. With **no** address, reset cannot help — there is nowhere to send the link — and the ways in are Facebook itself and support. | Generic banner; for the address-on-file fixture assert reset then password sign-in as `"homeowner"`; for the address-less fixture assert `POST /v1/auth/password-reset/request` still returns its uniform response and no mail is queued. Uniformity pinned by password_reset.rs:47; the address-less limitation is structural and **untested**. |
| Sign in with Google | **400** `invalid_request`, **FED-400**. | Nothing created, no session. | No Google identity on file; sign-in sends no side. Connect Google from the dashboard once signed in. | Banner *"No account here yet uses that Google sign-in"*; then Facebook sign-in + `POST /v1/auth/link/google` → 204, role still `"homeowner"`. Linking pinned by google.rs:247. |
| Sign in with Facebook (subject F) | **200** session. | Session as **homeowner**; login timestamp touched. | Rule 1 — same subject, same account. | `user.account_type == "homeowner"`; same `user.id` across repeat sign-ins. Creation pinned by `a_first_facebook_sign_in_creates_an_account_and_a_session` (google.rs:357); the returning-identity assertion is pinned on the Google side (:149), same function. |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409** when E is on file. | No new account; the Facebook account is unchanged. | Rule 2. **Fork:** the address-less Facebook account leaves E free, so this returns **202** and creates a second, separate homeowner account — the "two accounts, one human" case, reachable on purpose. | Assert 409 for the address-on-file fixture and 202 + two `user.id`s for the address-less one. Shared insert path; the **address-less fork has no dedicated test**, and `two_accounts_without_emails_do_not_collide` (google.rs:508) only pins that NULLs do not collide with each other. |
| Sign up as Contractor with email | **409** `conflict`, **REG-409** when E is on file. | No new account; the Facebook account is still a **homeowner**. | INSERT-only registration; requested side discarded with the failed row. Address-less fork → 202 and a separate contractor account. | Assert 409, then Facebook sign-in and assert `GET /v1/me` is `"homeowner"`. **No test**. |
| Sign up as Homeowner with Google (new subject) | **409** `conflict`, **FED-409** when the addresses match. | No new account, no Google identity row, no session. | New identity, colliding address, Rule 2 — no merge on a shared address. **Fork:** different or absent address → a separate homeowner account. | Banner **FED-409**; assert one users row and one identity row. Pinned by `a_shared_email_across_providers_is_a_conflict_not_a_merge` (google.rs:426). |
| Sign up as Contractor with Google (new subject) | **409** `conflict`, **FED-409** when the addresses match. | No new account; the Facebook account is still a homeowner. | As above; the requested side never survives the insert. | Banner **FED-409**; assert the stored role is untouched. **No test**. |
| Sign up as Homeowner with Facebook (subject F) | **200** session. | Session as **homeowner**; nothing written but the login timestamp. | Rule 4 — identity resolves, create branch skipped, field unread. | `user.account_type == "homeowner"`; users count unchanged. Pinned on the Google side (google.rs:614), same function. |
| Sign up as Contractor with Facebook (subject F) | **200** session — **not an error**. | Session as **homeowner**. The `contractor` field is discarded, unread. | Cross-role registration on the Facebook route. Same single function; `account_type` cannot re-side a resolved identity. | Send `account_type: "contractor"`, assert 200 and `user.account_type == "homeowner"`, and `POST /v1/contractors/{id}/claims` → **403**. Pinned on the Google side (google.rs:614); **no Facebook-route test**, so a per-provider fork here would pass CI. |

## 7 · Existing Contractor, Facebook account (subject F, address E)

One user row, `account_type='contractor'`, no password credential, one Facebook
identity for subject F. Address may be NULL.

| Attempt | System response | Resulting state | Why | E2E assertion target |
|---|---|---|---|---|
| Sign in with email | **401** `unauthenticated`. Decoy hash; audited `no_password_set`. | No session. Account unchanged. | `Precheck::NoPassword`. Reset grants a first password only if an address is on file; an address-less Facebook contractor has Facebook and support, nothing else. | Generic banner; address-on-file fixture: reset → password sign-in as `"contractor"` and `POST /v1/jobs` → 403. Uniform reset response pinned by password_reset.rs:47; the rest is **untested**. |
| Sign in with Google | **400** `invalid_request`, **FED-400**. | Nothing created, no session. | No Google identity; sign-in sends no side. Connect Google from the dashboard. | Banner names Google; then Facebook sign-in + link → 204 with role still `"contractor"`. Linking pinned by google.rs:247. |
| Sign in with Facebook (subject F) | **200** session. | Session as **contractor**; login timestamp touched. | Rule 1. | `user.account_type == "contractor"`; `POST /v1/jobs` → **403**; same `user.id` on repeat. Creation pinned by google.rs:357 (homeowner fixture); returning-identity on the Google side (:149). |
| Sign up as Homeowner with email | **409** `conflict`, **REG-409** when E is on file. | No new account; the Facebook account is still a **contractor**. | Rule 2 first; requested homeowner side discarded. Address-less fork → 202 and a second, separate homeowner account. | Assert 409, then Facebook sign-in and assert `GET /v1/me` is `"contractor"`. **No test**. |
| Sign up as Contractor with email | **409** `conflict`, **REG-409** when E is on file. | No new account; account unchanged, still without a password. | Rule 2; a matching side is still a duplicate address. Address-less fork → 202 and a separate contractor account. | Field error with **REG-409**; one users row. Shared insert path (auth.rs:91 from the email side). |
| Sign up as Homeowner with Google (new subject) | **409** `conflict`, **FED-409** when the addresses match. | No new account; contractor account untouched. | New identity, colliding address, Rule 2. Different/absent address → separate account. | Banner **FED-409**; one users row with `account_type='contractor'`. Pinned by google.rs:426. |
| Sign up as Contractor with Google (new subject) | **409** `conflict`, **FED-409** when the addresses match. | No new account; contractor account untouched. | As above. Also the cell that proves a matching side buys no merge. | Banner **FED-409**; assert one identity row (Facebook only). Cross-provider conflict pinned by google.rs:426; guard on token/route pairing by `each_endpoint_refuses_the_other_providers_token` (:388). |
| Sign up as Homeowner with Facebook (subject F) | **200** session — **not an error**. | Session as **contractor**. The `homeowner` field is discarded, unread. | Cross-role registration, Facebook route, contractor direction — the cell furthest from any existing test, and the one where a regression would be quietest. | Send `account_type: "homeowner"`, assert 200 and `user.account_type == "contractor"`; assert `POST /v1/jobs` → **403**; assert the stored column unchanged. **No test** on this route or this direction. |
| Sign up as Contractor with Facebook (subject F) | **200** session. | Session as **contractor**; nothing written but the login timestamp. | Rule 4; field agrees and is still not read. | `user.account_type == "contractor"`; users count unchanged. Pinned on the Google side (google.rs:614). |

---

## The shape of the 63 cells

Read down the columns and the table collapses to four outcomes, which is the
useful summary to keep in your head while writing specs:

| Outcome | Cells | Trigger |
|---|---|---|
| **200**, signed in, `account_type` ignored | 12 | The attempt's provider identity already resolves |
| **200** or **202**, new federated account on the chosen side | 4 | Federated sign-up where no identity resolves and the address is free |
| **202** challenge, new email account on the chosen side | 2 | Email sign-up where the address is genuinely free |
| **202** challenge, existing account signing in | 2 | Correct password on an unremembered browser |
| **400** FED-400 | 10 | Federated **sign-in** with no matching identity — regardless of who holds the address |
| **401** LOGIN-401 | 5 | Email sign-in against no account, or against an account with no password |
| **409** REG-409 / FED-409 | 28 | Any creation attempt whose address is already held |

Two things fall out of that count. The 10 four-hundreds are all one line of
code (`service.rs:1522`) and one product decision: a side can never be changed,
so it can never be guessed. The 28 four-oh-nines are all one database
constraint. Neither is spread across providers or roles, which is why the
matrix is large and the code under it is not.

## Off-matrix cells worth a spec anyway

- **Unparseable side.** `account_type: "landlord"` → **400** *"Account type must be one of: homeowner, contractor."*, raised in the handler before the token is verified or the rate limit is charged. Pinned by `an_unknown_account_type_is_refused_by_name` (google.rs:649).
- **No side chosen on `/signup`.** The client refuses before any network call: *"Choose whether you are a homeowner or a contractor."* Choosing a radio clears the banner. Assert zero requests to `/v1/auth/*`.
- **Wrong token for the route.** A Facebook token posted to `/v1/auth/google` is refused; pinned by `each_endpoint_refuses_the_other_providers_token` (google.rs:388).
- **Rate limits.** `federated_sign_in_per_ip`, `login_per_ip`, `register_per_ip`, `login_code_issue_per_user`, `login_code_verify_per_challenge` all answer **429** with `Retry-After`; the client renders *"Too many attempts. Try again in N seconds."* Any spec that loops a federated cell will hit these.
- **Linking.** `POST /v1/auth/link/{google,facebook}` → **204**, authenticated and CSRF-protected, one identity per provider per account. This is the only path that ever joins two identities, and it never touches `account_type`.

## Gaps this matrix exposed, and where they stand

1. ~~No test asserts `users.account_type` after a cross-role 409.~~ Pinned: `a_cross_side_duplicate_leaves_the_existing_account_on_its_side` (auth.rs).
2. ~~Cross-role sign-up is pinned in one direction only.~~ Pinned both ways on Google; the test loops a `PROVIDERS` const, so Facebook is one tuple away.
3. **The Facebook route has no side-handling test.** Deferred on purpose: Facebook sign-in is unpublished (`facebookLoginEnabled = false`). Add `("facebook.com", "/v1/auth/facebook")` to `PROVIDERS` in google.rs when the Meta app goes Live.
4. **Federated cells in the browser** are wired (`connectAuthEmulator` behind an env var; specs written and self-skipping) but not yet run: the Auth emulator needs Java. See [auth-state-machine.md](auth-state-machine.md) Phase 2.
5. ~~400-beats-409 is untested.~~ Pinned: `a_federated_sign_in_against_a_taken_address_is_refused_for_the_missing_side` (google.rs).

## Re-verifying the backend half

```bash
DATABASE_URL=... cargo test -p cm-api --test auth --test google --test login_codes --test password_reset
```
