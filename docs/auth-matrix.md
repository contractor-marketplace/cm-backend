# Authentication matrix

Every way in, from every starting state. Four account states × six attempts =
24 cells, each with what happens, why, and the test that pins it.

Evidence is the code as of `63a8828` (backend) — file references are to this
repository. Cells marked **shared path** are behaviours executed by the same
function as a pinned sibling cell but not driven by their own test; that is a
smaller claim than "tested" and is labelled honestly.

Two invariants decide most of this table:

1. **Identity is `(provider, subject)`, never an email address.** A federated
   sign-in is resolved only by the provider's own account id
   (`service.rs::sign_in_with_provider`, `oauth::find_by_subject`). An email on
   a token — however verified it claims to be — never finds an existing
   account. This is the defence against account takeover by anyone who can
   obtain a Google or Facebook account bearing your address.
2. **Email is a collision tripwire, not a key.** `users.email` is unique when
   present and nullable since 0035. Creating any account whose address is
   already held is a 409, never a merge. The sanctioned merge is provider
   linking, done signed-in from the account page ("Sign-ins" row of the
   dashboard).

Attempts are as the UI makes them: the **sign-in page** sends no
`account_type` with a provider token; the **sign-up page** sends the chosen
side (`AuthForm.tsx` `onProvider`). "Continue with Google/Facebook" hits one
endpoint either way — the two pages differ only in that field.

To re-verify the whole table:

```bash
DATABASE_URL=... cargo test -p cm-api --test auth --test google --test login_codes --test password_reset
```

## 1 · No existing account

| Situation | What happens, and why | Testing status |
|---|---|---|
| Sign in with email | **Working.** Unknown address is indistinguishable from a wrong password: a decoy hash burns the same time, the response is byte-identical 401 (`service.rs` `Precheck::NoAccount`). Tells an attacker nothing about which addresses exist. | `every_login_failure_looks_the_same` (auth.rs:186) |
| Sign in with Google | **Working.** Token verifies, no identity matches, no `account_type` was sent → 400: *"No account here yet uses that Google sign-in. Create an account first and choose whether you are a homeowner or a contractor."* Refused rather than assigned a side at random, because a side can never be changed later. | `federated_sign_in_without_an_account_refuses_rather_than_guessing` (google.rs:575) |
| Sign in with Facebook | **Working.** Same gate, same message with "Facebook" (`sign_in_with_provider` is one function; the route fixes the provider). | Shared path — pinned via Google; no Facebook-specific test |
| Create account with email | **Working.** 202 with a challenge; the emailed 6-digit code creates the session and marks the address verified. No session exists before the code. | `registering_returns_a_challenge_and_no_session`, `the_code_creates_a_session_and_verifies_the_address` (login_codes.rs:35, 66) |
| Create account with Google | **Working** (was the outage). Account created from the chosen side; email taken from the token's top-level claim, else its `identities` slot, else the popup's copy (stored unverified); display name from the token's signed `name` claim, else the popup's, else derived. | `a_first_google_sign_in_creates_an_account_and_a_session` (google.rs:120), `a_production_shaped_token_signs_up_via_the_identities_slot` (:672), `a_token_with_no_email_anywhere_accepts_the_popups_copy` (:697), `federated_sign_up_creates_the_side_the_person_chose` (:543), `the_tokens_name_claim_becomes_the_display_name` (:951) |
| Create account with Facebook | **Working**, including the genuinely email-less case (an account registered by phone number): since 0035 the account is created with no address, named "Facebook user", and adds an email from the account page when it wants notifications. | `a_first_facebook_sign_in_creates_an_account_and_a_session` (google.rs:357), `a_facebook_account_without_an_email_still_gets_in` (:477), `two_accounts_without_emails_do_not_collide` (:508) |

## 2 · Has an email (password) account

| Situation | What happens, and why | Testing status |
|---|---|---|
| Sign in with email | **Working.** Correct password on an unremembered browser → 202 code challenge, then session; a remembered browser skips the code. Eight failures lock the account even against the right password; the lock expires. | `login_issues_a_new_session_distinct_from_the_first` (auth.rs:161), `a_remembered_browser_logs_in_without_a_code`, `a_forged_device_cookie_still_gets_challenged` (login_codes.rs:102, 125), `eight_failures_lock_the_account_against_the_correct_password` (auth.rs:232) |
| Sign in with Google | **Working by design, deliberately not a match.** A Google account bearing this person's address is *not* given their account (invariant 1) — they get the same "no account uses that Google sign-in" refusal as a stranger. The sanctioned route is: sign in with email, then **Connect Google** on the dashboard's Sign-ins row (`link_provider`, `service.rs`). | Non-matching pinned by `a_google_account_is_never_matched_to_an_existing_account_by_email` (google.rs:182); linking by `linking_requires_being_signed_in_and_is_one_per_provider` (:247), `a_password_account_can_still_use_its_password_after_linking` (:338) |
| Sign in with Facebook | **Working by design.** Identical to the Google row. | Shared path; Facebook linking endpoint exists and is covered by the linking tests |
| Create account with email | **Working.** 409: *"That email address is already registered."* (`users.rs:215`). Nothing about the existing account leaks beyond the address being taken — which the person asserting it already knows. | `a_duplicate_address_is_refused` (auth.rs:91) |
| Create account with Google | **Working.** New Google identity, but the address collides → 409: *"An account already uses that email address. Sign in to that account instead."* — method-agnostic copy, because the colliding account may itself have no password (`service.rs:1540`). The password account is untouched; no second account is created. A Google account with a *different* address creates a separate account, by design. | `a_google_account_is_never_matched_to_an_existing_account_by_email` (google.rs:182), `a_client_address_colliding_with_an_existing_account_is_refused` (:752) |
| Create account with Facebook | **Working.** Same collision behaviour. | `a_shared_email_across_providers_is_a_conflict_not_a_merge` (google.rs:426) drives the cross-provider case; email-account collision is the shared insert path |

## 3 · Has a Google account

| Situation | What happens, and why | Testing status |
|---|---|---|
| Sign in with email | **Working by design.** The account has no password, so the attempt burns a decoy hash and fails with the same generic 401 as any bad login (`service.rs` `Precheck::NoPassword`, audited as `no_password_set`). Not a dead end: password reset **gives a federated account its first password** when its address is on file — after which both doors work. | Decoy path shape by `every_login_failure_looks_the_same` (auth.rs:186); the escape hatch by `a_reset_gives_a_federated_account_its_first_password` (google.rs:863) |
| Sign in with Google | **Working.** Same `(provider, subject)` → same account, every time. `account_type` is ignored for a returning identity. | `a_returning_google_user_gets_the_same_account` (google.rs:149) |
| Sign in with Facebook | **Working by design.** No Facebook identity on file → the "no account uses that Facebook sign-in" refusal. Route to having both: sign in with Google, **Connect Facebook** on the dashboard. | Shared path for the refusal; `the_account_page_knows_which_providers_are_connected` (google.rs:826) pins the dashboard's view |
| Create account with email | **Working.** Same address → 409 "already registered" via the unique index — the same insert path as section 2. If the Google account was created with *no* address (possible since 0035), the person's email is free and registration creates a second, separate account: two accounts, one human, resolved by linking or support — accepted by design. | Same-address case is the shared `users::insert` conflict path (pinned from the email side by `a_duplicate_address_is_refused`); the federated-address variant has no dedicated test |
| Create account with Google | **Working.** The sign-up page's "Continue with Google" resolves the existing identity and simply **signs them in** — one endpoint serves both pages, and `account_type` cannot re-side an existing account. This is exactly the sign-in → "create account first" → sign-up funnel; it now lands in the account instead of a dead end. | `the_account_type_field_cannot_re_type_an_existing_account` (google.rs:614) |
| Create account with Facebook | **Working.** A genuinely new Facebook identity: if its address matches the Google account's → 409 *"Sign in to that account instead."*; different or absent address → a separate account, by design. | `a_shared_email_across_providers_is_a_conflict_not_a_merge` (google.rs:426), `distinct_google_subjects_never_collapse_into_one_account` (:225) |

## 4 · Has a Facebook account

Mirror of section 3 with the providers swapped — the same single code path
serves both, with the provider fixed by the route and each endpoint refusing
the other's tokens.

| Situation | What happens, and why | Testing status |
|---|---|---|
| Sign in with email | **Working by design.** No password → decoy + generic 401; reset grants a first password when an address is on file. A Facebook account created with *no* address cannot use the reset (nothing to send the link to) — its ways in are Facebook itself, and support. | Shared path with section 3; the no-address limitation is structural (`password_reset.rs:47` pins that unknown addresses look identical) |
| Sign in with Google | **Working by design.** Refusal; connect Google from the dashboard once signed in. | Shared path |
| Sign in with Facebook | **Working.** Same subject → same account. | `a_first_facebook_sign_in_creates_an_account_and_a_session` (google.rs:357) covers creation; returning-identity behaviour pinned on the Google side (:149), same function |
| Create account with email | **Working.** As section 3: 409 on a shared address; a fresh address (or an address-less Facebook account) yields a second account by design. | Shared insert path |
| Create account with Facebook | **Working.** Resolves the existing identity and signs in; `account_type` ignored. | Pinned on the Google side (:614), same function |
| Create account with Google | **Working.** New Google identity; shared address → 409, otherwise a separate account. | `a_shared_email_across_providers_is_a_conflict_not_a_merge` (:426), `each_endpoint_refuses_the_other_providers_token` (:388) guards the token/route pairing |

## What "working by design" is admitting

Three journeys end in a refusal that is correct and still costs a person a
retry: an email-account holder pressing a provider button, and a federated
holder pressing the other provider's button or the email form. All three are
the price of invariant 1 — matching by email is exactly the takeover this
design refuses — and all three have a sanctioned route (sign in the way the
account knows, then connect the other method from the dashboard). The copy in
those refusals is where any future UX work belongs; the behaviour should not
move.

The known soft spots, named rather than hidden:

- **Facebook + no address + lost Facebook access** has no self-service
  recovery. Support is the path, and the account page nudges every address-less
  account to add one before that day comes.
- **Two accounts, one human** is reachable on purpose (different addresses
  across methods). Linking prevents it going forward; nothing merges two
  existing accounts today.
- **Shared-path cells** above trust that one function serves both providers.
  That is true today by inspection (`sign_in_with_provider` takes the provider
  as a parameter); a per-provider fork introduced later would not be caught by
  the Google-side tests alone.
