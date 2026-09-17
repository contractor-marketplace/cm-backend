# Authentication and claiming, as a state machine

Every state a person's account can be in, every action the product lets them
take from it, and where each action lands. Nodes are states; directed edges
are actions, labelled `action · HTTP status` and, where it matters, the side
effect. The tables after the diagrams list every node and every edge, so a
spec can be written from a single row.

Companions: [auth-matrix.md](auth-matrix.md) holds the reasoning behind the
four-state design; [auth-matrix-roles.md](auth-matrix-roles.md) holds the
63-cell sign-in/sign-up matrix with per-cell assertion targets. This document
is the graph those tables are projections of, plus the session, credential,
email, status and claim lifecycles they leave out.

Evidence is the code at `46356eb`. **Baseline, run on 2026-09-16 against the
Docker PostGIS, after Phase 1:** `auth` 21, `google` 29, `login_codes` 8,
`password_reset` 7, `security` 19, `claims` 8, `email_verify` 5 — **97 passed,
0 failed.** Nothing
in this document describes a behaviour that is currently broken; it describes
what is proved, what is true by inspection only, and what is not yet reachable
by a browser test. The path forward at the end is about closing those three
gaps, in that order.

## How to read the diagrams

- A node labelled **Refused NNN** is where an *attempt* ends. The account it
  was made against is unchanged; nothing was created, no session was issued.
- Four response messages recur and are abbreviated: **FED-400** (*"No account
  here yet uses that Google sign-in. Create an account first and choose whether
  you are a homeowner or a contractor — it cannot be changed later."*),
  **FED-409** (*"An account already uses that email address. Sign in to that
  account instead."*), **REG-409** (*"That email address is already
  registered."*), **LOGIN-401** (body *"Authentication is required."*; UI
  *"That email and password do not match an account."*).
- **E** is the one address every attempt uses. A federated attempt carries a
  *fresh* provider subject unless the diagram says it carries the account's
  own — that is the hostile question the graph is built to answer: same
  human, same address, different door.
- Every edge is numbered in the transition table (`A1.4`, `G.7` …).

## The state variables

An account's state is the product of these. Each diagram slices one or two of
them; the others are held fixed and named in the diagram's heading.

| Variable | Values | Stored in | Changed by |
|---|---|---|---|
| Existence | none · exists | `users` row | registration, federated first arrival |
| Side | `homeowner` · `contractor` | `users.account_type` | set at insert; **no write path afterwards** |
| Credentials | any non-empty subset of {password, Google, Facebook} | `password_credentials`, `oauth_identities` | link endpoints (add only — there is no unlink), password reset (adds password) |
| Email | none · unverified · verified | `users.email`, `users.email_verified_at` | login code, password reset, email change code, a provider's verified claim about the *same* address |
| Status | `active` · `suspended` · `deleted` | `users.status` | no endpoint — support/DB only |
| Lock | unlocked · locked (15 min) | `password_credentials.failed_attempts`, `locked_until` | 8 failures lock; time or a correct password after expiry clears |
| Browser | unremembered · remembered (90 d) | `__Host-cm_device` cookie | completing a code sets it; logout keeps it |
| Session | none · challenged (10 min, 5 tries) · signed in (idle 14 d, absolute 90 d) | `auth_tokens` (challenge), `sessions` | login, code, logout, logout-all, reset, expiry, status change |
| Claim (contractor side only) | none · pending · approved · withdrawn · rejected | `contractor_claims`, `contractors.claimed_by_user_id`, `user_roles` | open (auto-approves today), withdraw, moderator decide |

---

## A · Creation and matching

*Which account, if any, a sign-in or sign-up reaches. Session detail is in B
and C; this layer only says whether the attempt creates, matches, or refuses.*

### A1 · From nothing

```mermaid
stateDiagram-v2
    direction LR
    state "No account" as N0
    state "Homeowner · password" as HP
    state "Contractor · password" as CP
    state "Homeowner · Google" as HG
    state "Contractor · Google" as CG
    state "Homeowner · Facebook" as HF
    state "Contractor · Facebook" as CF
    state "Refused 401 LOGIN-401" as R401
    state "Refused 400 FED-400" as R400

    [*] --> N0
    N0 --> R401 : sign in with email · 401 decoy hash
    N0 --> R400 : sign in with Google · 400 no account_type
    N0 --> R400 : sign in with Facebook · 400 no account_type
    N0 --> HP : sign up homeowner with email · 202 then code
    N0 --> CP : sign up contractor with email · 202 then code
    N0 --> HG : sign up homeowner with Google · 200
    N0 --> CG : sign up contractor with Google · 200
    N0 --> HF : sign up homeowner with Facebook · 200
    N0 --> CF : sign up contractor with Facebook · 200
```

### A2 · From a password account

```mermaid
stateDiagram-v2
    direction LR
    state "Homeowner · password" as HP
    state "Contractor · password" as CP
    state "Challenged · code emailed" as CH
    state "Signed in as homeowner" as SH
    state "Signed in as contractor" as SC
    state "Refused 400 FED-400" as R400
    state "Refused 409 REG-409" as R409E
    state "Refused 409 FED-409" as R409F

    HP --> CH : sign in with email · 202 unremembered browser
    HP --> SH : sign in with email · 200 remembered browser
    HP --> R400 : sign in with Google · 400 (not 409)
    HP --> R400 : sign in with Facebook · 400 (not 409)
    HP --> R409E : sign up homeowner with email
    HP --> R409E : sign up contractor with email · side discarded
    HP --> R409F : sign up homeowner with Google
    HP --> R409F : sign up contractor with Google · side discarded
    HP --> R409F : sign up homeowner with Facebook
    HP --> R409F : sign up contractor with Facebook · side discarded

    CP --> CH : sign in with email · 202 unremembered browser
    CP --> SC : sign in with email · 200 remembered browser
    CP --> R400 : sign in with Google · 400 (not 409)
    CP --> R400 : sign in with Facebook · 400 (not 409)
    CP --> R409E : sign up homeowner with email · side discarded
    CP --> R409E : sign up contractor with email
    CP --> R409F : sign up homeowner with Google · side discarded
    CP --> R409F : sign up contractor with Google
    CP --> R409F : sign up homeowner with Facebook · side discarded
    CP --> R409F : sign up contractor with Facebook
```

### A3 · From a Google account (token carries the account's own subject where it says Google)

```mermaid
stateDiagram-v2
    direction LR
    state "Homeowner · Google" as HG
    state "Contractor · Google" as CG
    state "Signed in as homeowner" as SH
    state "Signed in as contractor" as SC
    state "Refused 401 LOGIN-401" as R401
    state "Refused 400 FED-400" as R400
    state "Refused 409 REG-409" as R409E
    state "Refused 409 FED-409" as R409F

    HG --> R401 : sign in with email · 401 no password set
    HG --> SH : sign in with Google · 200 same subject
    HG --> R400 : sign in with Facebook · 400
    HG --> R409E : sign up homeowner with email
    HG --> R409E : sign up contractor with email · side discarded
    HG --> SH : sign up homeowner with Google · 200 field unread
    HG --> SH : sign up contractor with Google · 200 field unread, stays homeowner
    HG --> R409F : sign up homeowner with Facebook · new subject
    HG --> R409F : sign up contractor with Facebook · new subject

    CG --> R401 : sign in with email · 401 no password set
    CG --> SC : sign in with Google · 200 same subject
    CG --> R400 : sign in with Facebook · 400
    CG --> R409E : sign up homeowner with email · side discarded
    CG --> R409E : sign up contractor with email
    CG --> SC : sign up homeowner with Google · 200 field unread, stays contractor
    CG --> SC : sign up contractor with Google · 200 field unread
    CG --> R409F : sign up homeowner with Facebook · new subject
    CG --> R409F : sign up contractor with Facebook · new subject
```

### A4 · From a Facebook account (token carries the account's own subject where it says Facebook)

```mermaid
stateDiagram-v2
    direction LR
    state "Homeowner · Facebook" as HF
    state "Contractor · Facebook" as CF
    state "Signed in as homeowner" as SH
    state "Signed in as contractor" as SC
    state "Refused 401 LOGIN-401" as R401
    state "Refused 400 FED-400" as R400
    state "Refused 409 REG-409" as R409E
    state "Refused 409 FED-409" as R409F

    HF --> R401 : sign in with email · 401 no password set
    HF --> R400 : sign in with Google · 400
    HF --> SH : sign in with Facebook · 200 same subject
    HF --> R409E : sign up homeowner with email
    HF --> R409E : sign up contractor with email · side discarded
    HF --> R409F : sign up homeowner with Google · new subject
    HF --> R409F : sign up contractor with Google · new subject
    HF --> SH : sign up homeowner with Facebook · 200 field unread
    HF --> SH : sign up contractor with Facebook · 200 field unread, stays homeowner

    CF --> R401 : sign in with email · 401 no password set
    CF --> R400 : sign in with Google · 400
    CF --> SC : sign in with Facebook · 200 same subject
    CF --> R409E : sign up homeowner with email · side discarded
    CF --> R409E : sign up contractor with email
    CF --> R409F : sign up homeowner with Google · new subject
    CF --> R409F : sign up contractor with Google · new subject
    CF --> SC : sign up homeowner with Facebook · 200 field unread, stays contractor
    CF --> SC : sign up contractor with Facebook · 200 field unread
```

The address-less fork: a Facebook (or Google) account created with
`email IS NULL` leaves E free, so its two "sign up with email" edges go to a
**new, separate account** (202) instead of REG-409, and the four
"sign up with the other provider" edges create a separate account (200)
instead of FED-409. Two accounts, one human, by design; linking prevents it
going forward and nothing merges them.

---

## B · Password session lifecycle

*For any account holding a password. Side, credentials and claim are fixed.*

```mermaid
stateDiagram-v2
    state "Signed out · browser unremembered" as OUT_U
    state "Signed out · browser remembered (90 d)" as OUT_R
    state "Challenged · code emailed (10 min · 5 tries)" as CH
    state "Signed in (idle 14 d · absolute 90 d)" as IN
    state "Locked (15 min after 8 failures)" as LOCK

    [*] --> CH : register · 202 account exists, no session
    [*] --> OUT_U
    OUT_U --> CH : correct password · 202
    OUT_R --> IN : correct password · 200 no code
    OUT_U --> OUT_U : wrong password 1 to 7 · 401
    OUT_R --> OUT_R : wrong password 1 to 7 · 401
    OUT_U --> LOCK : 8th wrong password · 401
    OUT_R --> LOCK : 8th wrong password · 401
    LOCK --> LOCK : any password, even the right one · 401 no hash work
    LOCK --> OUT_U : 15 minutes elapse
    LOCK --> OUT_R : 15 minutes elapse · device cookie untouched
    CH --> IN : correct code · 200 sets device cookie, verifies address
    CH --> CH : wrong code 1 to 4 · 401
    CH --> OUT_U : 5th wrong code · challenge dead
    CH --> OUT_U : 10 minutes elapse · reads as a wrong code
    CH --> CH : resend · 202 new challenge_id, old one dead
    IN --> OUT_R : logout · 204 session and csrf cleared, device kept
    IN --> OUT_R : logout-all · 204 every session revoked
    IN --> OUT_R : 14 d idle or 90 d absolute · next request 401
    IN --> OUT_R : password reset confirmed · every session revoked
    IN --> IN : change password · 204 other sessions revoked
    IN --> OUT_U : status leaves active · next request 401
    OUT_R --> OUT_U : 90 d elapse or forged device cookie
```

Rate limits sit on top of every edge into `CH` and `IN`: `login_per_ip`,
`register_per_ip`, `login_code_issue_per_user`,
`login_code_verify_per_challenge` → **429** with `Retry-After`.

## C · Federated session

*For any account holding a Google or Facebook identity. The token carries
that identity's subject.*

```mermaid
stateDiagram-v2
    state "Signed out" as OUT
    state "Signed in (idle 14 d · absolute 90 d)" as IN

    [*] --> OUT
    OUT --> IN : provider token resolves · 200 no code, no device cookie
    OUT --> OUT : token from the other provider on this route · refused
    OUT --> OUT : federated_sign_in_per_ip exceeded · 429
    OUT --> OUT : status not active · 401
    IN --> OUT : logout · 204
    IN --> OUT : logout-all · 204
    IN --> OUT : idle or absolute expiry · 401
    IN --> OUT : password reset confirmed · every session revoked
    IN --> OUT : status leaves active · next request 401
```

A federated sign-in never sets `__Host-cm_device`; the remembered-browser
concept belongs to the password path only. A returning identity whose token
carries a verified claim about *this account's own* address marks the address
verified on the way through (`service.rs:1560`).

## D · Credential set

*Signed in. Which doors an account has. Add-only: there is no unlink endpoint.*

```mermaid
stateDiagram-v2
    direction LR
    state "password" as P
    state "Google" as G
    state "Facebook" as F
    state "password + Google" as PG
    state "password + Facebook" as PF
    state "Google + Facebook" as GF
    state "password + Google + Facebook" as PGF
    state "Refused 409" as R409

    P --> PG : link Google · 204
    P --> PF : link Facebook · 204
    G --> GF : link Facebook · 204
    G --> PG : password reset with address on file · first password
    F --> GF : link Google · 204
    F --> PF : password reset with address on file · first password
    PG --> PGF : link Facebook · 204
    PF --> PGF : link Google · 204
    GF --> PGF : password reset with address on file · first password
    PG --> R409 : link Google again · This account already has a Google identity linked.
    PF --> R409 : link Facebook again
    P --> R409 : link an identity attached elsewhere · already linked to another account
    G --> G : password reset with no address on file · nothing to send to
    F --> F : password reset with no address on file · nothing to send to
```

Linking needs a session **and** a CSRF token; anonymous → 401. After
linking, the password still works (`a_password_account_can_still_use_its_password_after_linking`).

## E · Email address

```mermaid
stateDiagram-v2
    state "No address" as E0
    state "Unverified" as E1
    state "Verified" as E2
    state "Refused 409 REG-409" as R409

    [*] --> E1 : register with email · 202
    [*] --> E1 : federated first arrival · address from token, identities slot, or popup
    [*] --> E2 : federated first arrival · token claims verified and matches
    [*] --> E0 : federated first arrival · no address anywhere (Facebook)
    E1 --> E2 : login code completed · 200
    E1 --> E2 : password reset confirmed · 204
    E1 --> E2 : confirm the address on file · code · 200
    E1 --> E2 : returning federated sign-in · verified claim matches own address
    E0 --> E1 : request email change with new address · 202
    E0 --> E2 : confirm new address · 200
    E1 --> E2 : change to a new address · 202 then 200
    E2 --> E2 : change to a new address · 202 then 200
    E0 --> R409 : confirm an address another account holds
    E1 --> R409 : confirm an address another account holds
    E2 --> R409 : confirm an address another account holds
```

`email_verified` is reported by `/v1/me` and gates nothing today
(`grep email_verified handlers/` finds only the view). It is state, not
permission.

## F · Account status

```mermaid
stateDiagram-v2
    state "active" as A
    state "suspended" as S
    state "deleted" as D

    [*] --> A : any creation
    A --> S : support / DB only · no endpoint
    A --> D : support / DB only · no endpoint
    S --> A : support / DB only
    S --> S : any sign-in · 401 · existing sessions 401 on next request
    D --> D : any sign-in · 401 · existing sessions 401 on next request
```

`users::insert` never sets status; the column defaults to `active`
(migration 0003). `can_authenticate()` is `matches!(self, Active)` and is
checked on password login, federated login, and every authenticated request.

## G · Claim lifecycle

*Contractor-side accounts only. Signed in. `open()` auto-approves today
(`46356eb`); the pending branch is dormant but intact.*

```mermaid
stateDiagram-v2
    state "No claim" as NC
    state "Approved · owns listing · role contractor" as AP
    state "Pending (manual-review mode only)" as PE
    state "Withdrawn" as WD
    state "Rejected" as RJ
    state "Homeowner account" as HO
    state "Anonymous" as AN
    state "Refused 401" as R401
    state "Refused 403" as R403
    state "Refused 404" as R404
    state "Refused 409" as R409
    state "Refused 400" as R400

    [*] --> NC
    AN --> R401 : open · no session
    HO --> R403 : open · may_claim_a_listing false
    NC --> AP : open on an unclaimed listing · 201 auto-approved, role granted, badge recomputed
    NC --> R409 : open on a claimed listing · This listing has already been claimed.
    NC --> R409 : open while already owning one · Your account has already claimed a listing.
    NC --> R404 : open on an unknown listing
    NC --> R400 : open with an unknown method
    AP --> AP : open again · 409
    AP --> AP : withdraw · 409 That claim has already been decided.
    AP --> AP : licence goes inactive · badge removed, ownership kept
    AP --> AP : claimed on a dead licence · owns the page, never verified
    NC --> PE : open in manual mode · 201 pending
    PE --> AP : moderator approve · 200
    PE --> RJ : moderator reject · 200
    PE --> WD : withdraw · 204
    PE --> R409 : open the same listing again · You already have a claim pending on this listing.
    WD --> NC : nothing owned · may claim again
    RJ --> NC : nothing owned · may claim again
```

Two contractors opening the same listing at once both reach the approval
block; the partial unique index `contractor_claims_one_approved_per_contractor`
lets one `UPDATE` through and the other gets **409** — pinned by
`two_simultaneous_claims_produce_exactly_one_owner`. The moderation queue
(`GET /v1/admin/claims`, `POST …/decide`) answers **403** to anyone without
`admin` or `moderator`.

## H · Frontend routing

*What the browser shows for each backend state. `next` survives the code
screen (`AuthForm.tsx:255`), which is what makes the last edge of the
signed-out claim journey work.*

```mermaid
stateDiagram-v2
    state "Anonymous browser" as AN
    state "/login?next=…" as LOGIN
    state "/signup?next=…" as SIGNUP
    state "Code screen" as CODE
    state "/app · workspace" as APP
    state "/app/claim · find your business" as CLAIM0
    state "/app/claim?contractor=… · form" as CLAIMF
    state "Already claimed notice" as CLAIMED
    state "/app/listing" as LISTING
    state "/pros/slug · public listing" as PRO

    AN --> LOGIN : GET /app/* with no session cookie · middleware
    AN --> LOGIN : GET /app/* with a stale cookie · requireSession
    LOGIN --> CODE : password accepted · 202
    LOGIN --> APP : remembered browser or provider · afterSignIn(next)
    LOGIN --> LOGIN : 401 · banner, no field named
    LOGIN --> LOGIN : provider with no account · FED-400 banner
    SIGNUP --> SIGNUP : no side chosen · client refuses, no request sent
    SIGNUP --> CODE : email sign-up accepted · 202
    SIGNUP --> APP : provider sign-up · afterSignIn(next)
    SIGNUP --> SIGNUP : 409 · field error under email (REG-409) or banner (FED-409)
    CODE --> APP : code accepted · afterSignIn(next)
    CODE --> CODE : wrong code · retry, or resend
    PRO --> SIGNUP : Is this your business? signed out · next=/app/claim?contractor=slug
    PRO --> CLAIMF : Is this your business? contractor signed in
    PRO --> PRO : homeowner · CTA hidden
    PRO --> PRO : listing already claimed · CTA hidden
    CLAIM0 --> APP : homeowner account · redirect
    CLAIMF --> APP : homeowner account · redirect
    CLAIMF --> LISTING : already owns a listing · redirect
    CLAIMF --> APP : claim pending · redirect
    CLAIMF --> CLAIMED : listing is_claimed
    CLAIMF --> CLAIM0 : contractor param not found · redirect
    CLAIMF --> APP : submit · 201 · router.refresh shows Your listing
    CLAIMF --> CLAIMF : submit · 4xx · alert with the API message
    LISTING --> APP : no owned listing · redirect
    APP --> LISTING : Edit your listing
    APP --> PRO : Find your listing · Browse contractors
```

The `(app)` layout re-fetches `/v1/me` and `/v1/me/claims` on every server
render, so `isContractor` and `myContractorId` are correct immediately after
the claim form's `router.refresh()` — no client-side role cache to
invalidate.

---

## State table

| ID | State | What is true in the database | Reached by | Diagram |
|---|---|---|---|---|
| N0 | No account | no `users` row for E; no identity for the subject | initial | A1 |
| HP | Homeowner · password | `account_type='homeowner'`, `password_credentials` row, `email=E` | email sign-up as homeowner + code | A1, A2 |
| CP | Contractor · password | `account_type='contractor'`, password row, `email=E` | email sign-up as contractor + code | A1, A2 |
| HG | Homeowner · Google | `account_type='homeowner'`, `oauth_identities(google, G)`, no password | Google sign-up as homeowner | A1, A3 |
| CG | Contractor · Google | `account_type='contractor'`, `oauth_identities(google, G)`, no password | Google sign-up as contractor | A1, A3 |
| HF | Homeowner · Facebook | `account_type='homeowner'`, `oauth_identities(facebook, F)`, no password, email may be NULL | Facebook sign-up as homeowner | A1, A4 |
| CF | Contractor · Facebook | `account_type='contractor'`, `oauth_identities(facebook, F)`, no password, email may be NULL | Facebook sign-up as contractor | A1, A4 |
| CH | Challenged | `auth_tokens` row, purpose `LoginCode`, 10-min TTL, ≤5 attempts; `email_outbox` row queued; **no session** | register, or correct password on an unremembered browser | A2, B |
| SH | Signed in as homeowner | `sessions` row; cookies `__Host-cm_session`, `__Host-cm_csrf`; body `user.account_type="homeowner"` | any 200 for a homeowner | A2–A4 |
| SC | Signed in as contractor | as SH with `"contractor"` | any 200 for a contractor | A2–A4 |
| OUT_U | Signed out, unremembered | no session; no `__Host-cm_device` for this account | initial, lock expiry, 5th wrong code, code TTL, device TTL | B |
| OUT_R | Signed out, remembered | no session; valid `__Host-cm_device` (90 d, HMAC over user id) | logout, expiry, reset, from a signed-in remembered browser | B |
| IN | Signed in | `sessions` row with `idle_expires_at` (sliding, 14 d) and `absolute_expires_at` (90 d) | correct code; correct password on a remembered browser; provider token | B, C |
| LOCK | Locked | `failed_attempts ≥ 8`, `locked_until` 15 min ahead; hash not verified while locked | 8th wrong password | B |
| R400 | Refused 400 | unchanged | FED-400, unknown `account_type`, unknown claim method | A, G |
| R401 | Refused 401 | unchanged; audited `unknown_account` / `no_password_set` / `account_locked` / `bad_password` / `account_not_active` | LOGIN-401, anonymous mutation | A, B, C, G |
| R403 | Refused 403 | unchanged | wrong side for the capability; non-moderator on admin | G |
| R404 | Refused 404 | unchanged | unknown listing; someone else's claim on withdraw | G |
| R409E | Refused 409 REG-409 | unchanged; `users_email_norm_key` | email registration on a held address | A |
| R409F | Refused 409 FED-409 | unchanged; same index, rewritten message | federated creation on a held address | A |
| R409 | Refused 409 | unchanged | second link of a provider; identity linked elsewhere; claim conflicts | D, G |
| P / G / F | one credential | one of password, Google, Facebook | creation | D |
| PG / PF / GF | two credentials | password+Google, password+Facebook, Google+Facebook | one link, or a first password | D |
| PGF | three credentials | all three | two links, or a link and a first password | D |
| E0 | No address | `users.email IS NULL` | Facebook (or Google) first arrival with no address anywhere | E |
| E1 | Unverified | `email` set, `email_verified_at IS NULL` | registration; federated arrival without a matching verified claim; a pending change | E |
| E2 | Verified | `email_verified_at` set | login code; reset; confirm code; matching verified provider claim | E |
| A / S / D | active · suspended · deleted | `users.status` | default active; support only | F |
| NC | No claim | no `contractor_claims` row for the user; `roles` without `contractor` | contractor account at creation; after withdraw/reject | G |
| AP | Approved | claim `status='approved'`, `decided_by = claimant` (auto) or moderator; `contractors.claimed_by_user_id` set; `user_roles` has `contractor`; `verified` recomputed from licence | open (today); moderator approve (manual mode) | G |
| PE | Pending | claim `status='pending'`, `decided_at IS NULL`; partial unique per (contractor, user) | open, only with the auto-approval block removed | G |
| WD | Withdrawn | `status='withdrawn'`, decided by the claimant | withdraw a pending claim | G |
| RJ | Rejected | `status='rejected'`, decided by a moderator | moderator reject | G |
| AN / HO | Anonymous / Homeowner actor | — | actors whose only claim edge is a refusal | G |
| LOGIN, SIGNUP, CODE, APP, CLAIM0, CLAIMF, CLAIMED, LISTING, PRO | pages | see H | routing | H |

## Transition table

Columns: edge · from · action · guard · to · response · pinned by. "**by
inspection**" means the code path is one a sibling row tests, without a test
of its own.

### A1 · From nothing

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| A1.1 | N0 | sign in with email | — | R401 | 401, decoy hash, `unknown_account` | `every_login_failure_looks_the_same` (auth.rs:186) |
| A1.2 | N0 | sign in with Google | no `account_type` | R400 | 400 FED-400; zero rows | `federated_sign_in_without_an_account_refuses_rather_than_guessing` (google.rs:575) |
| A1.3 | N0 | sign in with Facebook | no `account_type` | R400 | 400 FED-400 (Facebook) | by inspection |
| A1.4 | N0 | sign up homeowner with email | valid input | HP (via CH) | 202 challenge; row exists | `registering_returns_a_challenge_and_no_session`, `the_code_creates_a_session_and_verifies_the_address` (login_codes.rs:35, 66) |
| A1.5 | N0 | sign up contractor with email | valid input | CP (via CH) | 202 challenge | by inspection (helper `register_contractor` is a fixture elsewhere) |
| A1.6 | N0 | sign up homeowner with Google | `account_type=homeowner` | HG + SH | 200 | `federated_sign_up_creates_the_side_the_person_chose` (google.rs:543), `a_first_google_sign_in_creates_an_account_and_a_session` (:120) |
| A1.7 | N0 | sign up contractor with Google | `account_type=contractor` | CG + SC | 200; exactly one contractor row | google.rs:543 |
| A1.8 | N0 | sign up homeowner with Facebook | `account_type=homeowner` | HF + SH | 200; email may be NULL | `a_first_facebook_sign_in_creates_an_account_and_a_session` (:357), `a_facebook_account_without_an_email_still_gets_in` (:477) |
| A1.9 | N0 | sign up contractor with Facebook | `account_type=contractor` | CF + SC | 200 | by inspection |

### A2 · From a password account

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| A2.1 | HP | sign in with email | correct password, unremembered | CH | 202 | `login_issues_a_new_session_distinct_from_the_first` (auth.rs:161) |
| A2.2 | HP | sign in with email | correct password, remembered | SH | 200 | `a_remembered_browser_logs_in_without_a_code` (login_codes.rs:102) |
| A2.3 | HP | sign in with Google | fresh subject, address E, no `account_type` | R400 | 400 FED-400 — **not 409** | `a_federated_sign_in_against_a_taken_address_is_refused_for_the_missing_side` (google.rs); non-match by :182 |
| A2.4 | HP | sign in with Facebook | as A2.3 | R400 | 400 FED-400 | by inspection |
| A2.5 | HP | sign up homeowner with email | address E | R409E | 409 REG-409 | `a_duplicate_address_is_refused` (auth.rs:91) |
| A2.6 | HP | sign up contractor with email | address E | R409E | 409 REG-409; **still homeowner** | `a_cross_side_duplicate_leaves_the_existing_account_on_its_side` (auth.rs) |
| A2.7 | HP | sign up homeowner with Google | fresh subject, address E | R409F | 409 FED-409 | google.rs:182, `a_client_address_colliding_with_an_existing_account_is_refused` (:752) |
| A2.8 | HP | sign up contractor with Google | fresh subject, address E | R409F | 409 FED-409; still homeowner | insert path by :752; contractor-side variant **untested** |
| A2.9 | HP | sign up homeowner with Facebook | fresh subject, address E | R409F | 409 FED-409 | `a_shared_email_across_providers_is_a_conflict_not_a_merge` (:426) |
| A2.10 | HP | sign up contractor with Facebook | fresh subject, address E | R409F | 409 FED-409; still homeowner | by inspection |
| A2.11 | CP | sign in with email | correct, unremembered | CH | 202 | auth.rs:161 shape |
| A2.12 | CP | sign in with email | correct, remembered | SC | 200 | login_codes.rs:102 shape |
| A2.13 | CP | sign in with Google | fresh subject, no `account_type` | R400 | 400 FED-400 | same test as A2.3 (the fixture side is irrelevant to the ordering) |
| A2.14 | CP | sign in with Facebook | as A2.13 | R400 | 400 FED-400 | by inspection |
| A2.15 | CP | sign up homeowner with email | address E | R409E | 409 REG-409; **still contractor** | `a_cross_side_duplicate_leaves_the_existing_account_on_its_side` (auth.rs), second iteration |
| A2.16 | CP | sign up contractor with email | address E | R409E | 409 REG-409 | auth.rs:91 |
| A2.17 | CP | sign up homeowner with Google | fresh subject, address E | R409F | 409 FED-409; still contractor | insert path by :752 |
| A2.18 | CP | sign up contractor with Google | fresh subject, address E | R409F | 409 FED-409 | :752 |
| A2.19 | CP | sign up homeowner with Facebook | fresh subject, address E | R409F | 409 FED-409 | :426 |
| A2.20 | CP | sign up contractor with Facebook | fresh subject, address E | R409F | 409 FED-409 | by inspection |

### A3 · From a Google account

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| A3.1 | HG | sign in with email | any password | R401 | 401, `no_password_set`; reset grants a first password | auth.rs:186; `a_reset_gives_a_federated_account_its_first_password` (google.rs:863) |
| A3.2 | HG | sign in with Google | subject G | SH | 200, same `user.id` | `a_returning_google_user_gets_the_same_account` (:149) |
| A3.3 | HG | sign in with Facebook | fresh subject, no `account_type` | R400 | 400 FED-400 | by inspection; linking route by `the_account_page_knows_which_providers_are_connected` (:826) |
| A3.4 | HG | sign up homeowner with email | address E on file | R409E | 409 REG-409 (address-less fork: 202, second account) | shared insert path; fork by `an_address_less_federated_account_reserves_no_address` (google.rs) |
| A3.5 | HG | sign up contractor with email | address E on file | R409E | 409 REG-409; still homeowner | same `users::insert` path as A2.6, pinned there from the password side |
| A3.6 | HG | sign up homeowner with Google | subject G | SH | 200; field unread | :149 |
| A3.7 | HG | sign up contractor with Google | subject G | SH | **200, still homeowner** | `the_account_type_field_cannot_re_type_an_existing_account` (:614) |
| A3.8 | HG | sign up homeowner with Facebook | fresh subject, address E | R409F | 409 FED-409 | :426 |
| A3.9 | HG | sign up contractor with Facebook | fresh subject, address E | R409F | 409 FED-409 | by inspection |
| A3.10 | CG | sign in with email | any password | R401 | 401 | auth.rs:186 shape |
| A3.11 | CG | sign in with Google | subject G | SC | 200 | :149 shape |
| A3.12 | CG | sign in with Facebook | fresh subject | R400 | 400 FED-400 | by inspection |
| A3.13 | CG | sign up homeowner with email | address E | R409E | 409; still contractor | same insert path as A2.15, pinned there |
| A3.14 | CG | sign up contractor with email | address E | R409E | 409 | shared insert path |
| A3.15 | CG | sign up homeowner with Google | subject G | SC | **200, still contractor** | `the_account_type_field_cannot_re_type_an_existing_account` (google.rs), now both directions |
| A3.16 | CG | sign up contractor with Google | subject G | SC | 200 | :149 shape |
| A3.17 | CG | sign up homeowner with Facebook | fresh subject, address E | R409F | 409 | :426 |
| A3.18 | CG | sign up contractor with Facebook | fresh subject, address E | R409F | 409 | by inspection |

### A4 · From a Facebook account

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| A4.1 | HF | sign in with email | any password | R401 | 401; reset works only with an address on file | auth.rs:186; password_reset.rs:47 for uniformity |
| A4.2 | HF | sign in with Google | fresh subject | R400 | 400 FED-400 | by inspection; linking by :247 |
| A4.3 | HF | sign in with Facebook | subject F | SH | 200 | creation :357; returning pinned on Google (:149), same function |
| A4.4 | HF | sign up homeowner with email | E on file | R409E | 409 (address-less fork: 202, second account) | shared path; fork pinned on the Google route |
| A4.5 | HF | sign up contractor with email | E on file | R409E | 409; still homeowner | same insert path as A2.6, pinned there |
| A4.6 | HF | sign up homeowner with Google | fresh subject, address E | R409F | 409 | :426 |
| A4.7 | HF | sign up contractor with Google | fresh subject, address E | R409F | 409 | by inspection |
| A4.8 | HF | sign up homeowner with Facebook | subject F | SH | 200; field unread | :614 on Google, same function |
| A4.9 | HF | sign up contractor with Facebook | subject F | SH | **200, still homeowner** | **deferred until Facebook ships**: add `("facebook.com", "/v1/auth/facebook")` to `PROVIDERS` in google.rs and the looped test covers it |
| A4.10 | CF | sign in with email | any password | R401 | 401 | shape only |
| A4.11 | CF | sign in with Google | fresh subject | R400 | 400 | by inspection |
| A4.12 | CF | sign in with Facebook | subject F | SC | 200 | shape only |
| A4.13 | CF | sign up homeowner with email | E on file | R409E | 409; still contractor | same insert path as A2.15, pinned there |
| A4.14 | CF | sign up contractor with email | E on file | R409E | 409 | shared path |
| A4.15 | CF | sign up homeowner with Google | fresh subject, address E | R409F | 409 | :426 |
| A4.16 | CF | sign up contractor with Google | fresh subject, address E | R409F | 409; token/route pairing by `each_endpoint_refuses_the_other_providers_token` (:388) | :426, :388 |
| A4.17 | CF | sign up homeowner with Facebook | subject F | SC | **200, still contractor** | **deferred until Facebook ships** — same `PROVIDERS` entry as A4.9 |
| A4.18 | CF | sign up contractor with Facebook | subject F | SC | 200 | shape only |

### B · Password session

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| B.1 | — | register | valid input, free address | CH | 202; row exists, no session | login_codes.rs:35 |
| B.2 | OUT_U | login | correct password | CH | 202 | auth.rs:161 |
| B.3 | OUT_R | login | correct password, valid device cookie | IN | 200 | login_codes.rs:102 |
| B.4 | OUT_* | login | wrong password, failures < 8 | OUT_* | 401 `bad_password` | auth.rs:186 |
| B.5 | OUT_* | login | 8th wrong password | LOCK | 401; audit `auth.account_locked` | `eight_failures_lock_the_account_against_the_correct_password` (auth.rs:232), `a_lockout_is_recorded_with_its_cause` (security.rs) |
| B.6 | LOCK | login | any password while locked | LOCK | 401 `account_locked`, hash **not** verified | auth.rs:232 |
| B.7 | LOCK | time | 15 min elapse | OUT_* | — | by inspection (`LOCKOUT_MINUTES`) |
| B.8 | CH | verify | correct code | IN | 200 + `__Host-cm_device`; address verified; failures cleared | login_codes.rs:66 |
| B.9 | CH | verify | wrong code, attempts < 5 | CH | 401 | `a_wrong_code_five_times_kills_the_challenge` |
| B.10 | CH | verify | 5th wrong code | OUT_U | 401; challenge dead | same |
| B.11 | CH | verify | after 10 min | OUT_U | 401, identical to wrong | `an_expired_code_reads_the_same_as_a_wrong_one` |
| B.12 | CH | resend | — | CH | 202 new id; old id and code dead | `a_new_code_invalidates_the_previous_one`; `code_issue_is_rate_limited_per_account` |
| B.13 | OUT_* | login | forged device cookie | CH | 202 (treated as unremembered) | `a_forged_device_cookie_still_gets_challenged` |
| B.14 | IN | logout | session + CSRF | OUT_R | 204; session and csrf cookies expired; device kept | handlers/auth.rs:417 |
| B.15 | IN | logout-all | session + CSRF | OUT_R | 204; every session revoked | by inspection |
| B.16 | IN | time | 14 d idle or 90 d absolute | OUT_R | next request 401 | `a_forged_or_stale_session_cookie_is_refused` (security.rs) |
| B.17 | IN | password reset confirm | valid link | OUT_R | 204; every session revoked; address verified | `a_reset_link_works_exactly_once_and_signs_out_every_session`, `a_reset_verifies_the_address_too` |
| B.18 | IN | change password | correct current | IN | 204; other sessions revoked | `a_password_change_records_what_it_revoked`, `password_change_is_limited_per_account` |
| B.19 | IN | status leaves active | — | OUT | next request 401 | `a_suspended_account_cannot_log_in_or_use_an_existing_session` (auth.rs:210) |
| B.20 | OUT_R | time / tamper | 90 d, or cookie altered | OUT_U | — | `DEVICE_TTL`; B.13 |
| B.21 | any | login / register / code | limit exceeded | same | 429 `Retry-After` | `login_is_limited_per_address_across_accounts`, `registration_is_limited_per_address`, login_codes rate tests |

### C · Federated session

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| C.1 | OUT | POST /v1/auth/{google,facebook} | token resolves, status active | IN | 200; no device cookie | google.rs:149, :357 |
| C.2 | OUT | same | other provider's token on this route | OUT | refused | `each_endpoint_refuses_the_other_providers_token` (:388) |
| C.3 | OUT | same | `federated_sign_in_per_ip` exceeded | OUT | 429 | by inspection |
| C.4 | OUT | same | status not active | OUT | 401 | `a_suspended_federated_account_cannot_sign_in_or_keep_its_session` (google.rs) |
| C.5 | IN | logout / logout-all / expiry / reset / status | — | OUT | as B.14–B.19 | shared |
| C.6 | OUT | same | Firebase not configured | OUT | 503 with a clear message | `google_sign_in_reports_clearly_when_it_is_not_configured` (:322) |

### D · Credential set

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| D.1 | P | link Google | signed in + CSRF | PG | 204 | `linking_requires_being_signed_in_and_is_one_per_provider` (:247), `linking_is_csrf_protected_like_any_other_mutation` (:302) |
| D.2 | P | link Facebook | same | PF | 204 | :247 shape |
| D.3 | G | link Facebook | same | GF | 204 | :826 (dashboard view) |
| D.4 | G / F / GF | password reset | address on file | +P | 204; first password; every session revoked | `a_reset_gives_a_federated_account_its_first_password` (:863) |
| D.5 | PG / PF / PGF | link same provider again | — | same | 409 *"This account already has a … identity linked."* | :247 |
| D.6 | any | link an identity attached to another account | — | same | 409 *"That … account is already linked to another account."* | :247 |
| D.7 | any | link | anonymous | same | 401 | :247 |
| D.8 | PG | login with password | — | IN | still works after linking | `a_password_account_can_still_use_its_password_after_linking` (:338) |
| D.9 | G / F | password reset | no address on file | same | 204 uniform response; nothing sent | `requesting_a_reset_for_an_unknown_email_looks_identical_to_a_known_one` for uniformity; the no-address case **untested** |

### E · Email

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| E.1 | — | register | — | E1 | 202 | login_codes.rs:35 |
| E.2 | E1 | verify login code | — | E2 | 200 | login_codes.rs:66 |
| E.3 | — | federated first arrival | token claim / identities / popup | E1 | stored unverified | `a_production_shaped_token_signs_up_via_the_identities_slot` (:672), `a_token_with_no_email_anywhere_accepts_the_popups_copy` (:697), `the_tokens_address_outranks_the_browsers` (:730) |
| E.4 | — | federated first arrival | verified claim matching stored address | E2 | verified at creation | by inspection (service.rs:1560) |
| E.5 | — | federated first arrival | no address anywhere | E0 | row with NULL email | :477, `two_accounts_without_emails_do_not_collide` (:508) |
| E.6 | E0 / E1 / E2 | request email change (new address) | signed in | pending | 202 challenge | email_verify.rs (5 tests) |
| E.7 | pending | confirm | correct code, address free | E2 | 200 user view | email_verify.rs |
| E.8 | pending | confirm | address held elsewhere | same | 409 REG-409 | email_verify.rs |
| E.9 | E1 | request confirm (no new address) | signed in | E2 after code | 202 then 200 | email_verify.rs |
| E.10 | E1 | password reset confirm | — | E2 | 204 | `a_reset_verifies_the_address_too` |
| E.11 | E1 | returning federated sign-in | verified claim matches own address | E2 | 200 | by inspection; `a_client_address_is_ignored_for_a_returning_identity` (:797) pins that the *client* copy never does this |

### F · Status

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| F.1 | A | support sets status | — | S / D | — | no endpoint |
| F.2 | S / D | any sign-in | — | same | 401 `account_not_active` | password: `a_suspended_account_cannot_log_in_or_use_an_existing_session` (auth.rs:210); federated: google.rs suspended test |
| F.3 | S / D | any authenticated request | existing session | same | 401 | both tests above assert the live session dies on its next `/v1/me` |

### G · Claims

| Edge | From | Action | Guard | To | Response | Pinned by |
|---|---|---|---|---|---|---|
| G.1 | AN | POST /v1/contractors/{id}/claims | no session | R401 | 401 | `a_claim_needs_a_session_and_moderation_needs_a_role` (claims.rs:194) |
| G.2 | HO | same | `account_type=homeowner` | R403 | 403 | handler claims.rs:35 + DB trigger; **verify claims.rs:194 covers it, else add** |
| G.3 | NC | same | listing unclaimed, user owns none | AP | 201; approved by claimant; role granted; ownership; badge recomputed; audit `auto_approved` | `an_approved_claim_grants_ownership_and_the_badge` (:19), `an_auto_approval_is_auditable_end_to_end` (:330) |
| G.4 | NC | same | listing already claimed | R409 | 409 *"This listing has already been claimed."* | `a_second_claim_on_a_claimed_listing_is_refused` (:296) |
| G.5 | NC | same | user already owns a listing | R409 | 409 *"Your account has already claimed a listing."* | by inspection (domain claims.rs:50) |
| G.6 | NC | same | unknown listing | R404 | 404 | by inspection |
| G.7 | NC | same | unknown `method` | R400 | 400 naming the options | by inspection (handler :41) |
| G.8 | NC × 2 | same, concurrently | same listing | one AP, one R409 | 201 + 409 | `two_simultaneous_claims_produce_exactly_one_owner` (:144) |
| G.9 | AP | withdraw | status ≠ pending | AP | 409 *"That claim has already been decided."* | `withdrawal_applies_only_to_pending_claims` (:255) |
| G.10 | AP | licence lapses | — | AP | badge removed, ownership kept | `a_licence_going_inactive_removes_the_badge` (:102) |
| G.11 | NC | open | licence already expired | AP | 201; owns page; `verified=false` | `an_expired_licence_is_never_verified` (:67) |
| G.12 | NC | open | manual mode (approval block removed) | PE | 201 pending | dormant; by inspection |
| G.13 | PE | POST /v1/admin/claims/{id}/decide approve | moderator | AP | 200 | dormant path; moderation role gate by :194 |
| G.14 | PE | decide reject | moderator | RJ | 200 | dormant |
| G.15 | PE | withdraw | own claim | WD | 204 | :255 (pending half) |
| G.16 | PE | open same listing again | — | R409 | 409 *"You already have a claim pending on this listing."* | dormant; repo claims.rs:135 |
| G.17 | WD / RJ | open | nothing owned | AP / PE | as G.3 | by inspection |
| G.18 | non-moderator | GET /v1/admin/claims | — | R403 | 403 | :194 |

### H · Routing

| Edge | From | Action | Guard | To | Pinned by |
|---|---|---|---|---|---|
| H.1 | AN | GET /app/* | no session cookie | /login?next= | middleware.ts (no test) |
| H.2 | AN | GET /app/* | stale cookie | /login?next= | session.ts `requireSession` (no test) |
| H.3 | LOGIN | submit | 202 | CODE | `sign-in-result.test.ts` (unit) |
| H.4 | LOGIN / SIGNUP / CODE | success | — | `afterSignIn(next)` → /app or `next` | redirects.ts (unit-testable, `safeNext` guards open redirects) |
| H.5 | SIGNUP | provider button | no radio chosen | SIGNUP | client guard, no request — **no test** |
| H.6 | SIGNUP | submit | 409 | SIGNUP | field error (REG-409) or banner (FED-409) — **no test** |
| H.7 | PRO | Is this your business? | signed out | /signup?next=/app/claim?contractor=slug | ContractorProfile.tsx:323 — **no test** |
| H.8 | PRO | same | homeowner | hidden | — |
| H.9 | PRO | same | contractor signed in | /app/claim?contractor=slug | — |
| H.10 | CLAIMF | render | homeowner / owns one / pending / not found / is_claimed | /app · /app/listing · /app · /app/claim · notice | claim/page.tsx:32–38 — **no test** |
| H.11 | CLAIMF | submit | 201 | /app with *"Your listing"* + *"Edit your listing"* | ClaimForm.tsx:47 — **no test** |
| H.12 | LISTING | render | no owned listing | /app | listing/page.tsx:22 |

---

## Path forward

The goal is a stated one — *completely working authentication and claiming,
end to end* — so it needs a definition of done that is checkable rather than
felt:

1. Every edge above marked **untested** or **by inspection** on a
   security-relevant path has a backend test that fails if it changes.
2. The nine browser journeys in Phase 3 run green in CI against a local
   stack (API + Next + PostGIS + Firebase Auth emulator).
3. The production checklist in Phase 5 is ticked and recorded.

Nothing below is a bug fix. The baseline is green and every documented
behaviour is the intended one. What is missing is *proof* for the cells that
are true by inspection, and *reachability* for the cells a browser cannot
currently drive.

### Phase 0 · Baseline — done today

93 backend tests across the seven suites, 0 failures, on the Docker PostGIS at
`localhost:55432`. Command at the bottom.

### Phase 1 · Pin what is true by inspection — done for email + Google

Landed 2026-09-16. Scope was deliberately email and Google: Facebook is
unpublished (`facebookLoginEnabled`), so every provider test loops over a
`PROVIDERS` const in `google.rs` with one entry; shipping Facebook means
adding one tuple there. Rows marked *deferred* wait on that.

| # | Test | Edges it pins | Status |
|---|---|---|---|
| 1 | `auth.rs::a_cross_side_duplicate_leaves_the_existing_account_on_its_side` — both directions, `SELECT account_type` after the 409 | A2.6, A2.15, A3.5, A3.13, A4.5, A4.13 | **done** |
| 2 | `google.rs::the_account_type_field_cannot_re_type_an_existing_account` loops both directions × `PROVIDERS` | A3.7, A3.15 (A4.9, A4.17 when Facebook is added to `PROVIDERS`) | **done** for Google |
| 3 | `google.rs::a_federated_sign_in_against_a_taken_address_is_refused_for_the_missing_side` | A2.3, A2.13 (A2.4, A2.14 via `PROVIDERS`) | **done** |
| 4 | Facebook route side handling | A1.9, A2.10, A2.20 | *deferred* — one `PROVIDERS` tuple when the Meta app is Live |
| 5 | `google.rs::an_address_less_federated_account_reserves_no_address` | A3.4, A4.4 fork | **done** |
| 6 | Homeowner `open` → 403 | G.2 | **already pinned** by `claims.rs::a_claim_needs_a_session_and_moderation_needs_a_role` |
| 7 | `google.rs::a_suspended_federated_account_cannot_sign_in_or_keep_its_session`; password side already `auth.rs:210` | C.4, F.2, F.3, B.19 | **done** |
| 8 | Reset request for an address-less account | D.9 | *not added* — indistinguishable from `requesting_a_reset_for_an_unknown_email_looks_identical_to_a_known_one`, which already pins the uniform 204 |

`cargo test -p cm-api --test auth --test google`: 21 + 29, green.

### Phase 2 · Make the stack reachable from a browser — done, emulator pending Java

Landed 2026-09-16 in cm-frontend:

- `lib/firebase.ts` calls `connectAuthEmulator` when
  `NEXT_PUBLIC_FIREBASE_AUTH_EMULATOR_HOST` is set (guarded on
  `emulatorConfig`, since the SDK throws on a repeat call after a sign-in).
- `tests/e2e/support/db.ts` reads the login code from `email_outbox` and
  picks unclaimed fixture listings, through `pg` — no test-only endpoint on
  the API.
- `playwright.config.ts` gains an **opt-in** `webServer` pair under
  `E2E_LOCAL_STACK=1`: the API on 8081 (waits on `/readyz`) and Next on 3100,
  both reused if already running. Env in `tests/e2e/local-stack.env`.
- `scripts/e2e-seed.sh` creates `cm_e2e` on the test PostGIS container,
  migrates, seeds trades and LA regions, and imports
  `cm-backend/deploy/data/e2e-licenses.csv` (25 real LA listings). `--fresh`
  drops and rebuilds it.
- `tests/e2e/global-setup.ts` clears `rate_limit_counters`: registration is
  10/hour per address and the whole suite is one address.

Still open: the Firebase Auth emulator needs a Java runtime, which the dev
machine does not have. The wiring is complete and the Google specs skip
themselves until the four emulator lines in `local-stack.env` are
uncommented on a machine that can run `firebase emulators:start --only auth`.

The original plan, for reference:

1. **`cm-frontend/lib/firebase.ts`**: after `getAuth(app)`, if
   `NEXT_PUBLIC_FIREBASE_AUTH_EMULATOR_HOST` is set, call
   `connectAuthEmulator(instance, \`http://${host}\`, { disableWarnings: true })`.
   Four lines, no test-only code path, and the emulator's own popup renders a
   fake account chooser that Playwright can drive with `page.waitForEvent("popup")`
   → *"Add new account"* → fill name and email → *"Sign in"*. Both Google
   and Facebook providers are served by the same fake page.
2. **Login codes in the browser**: the backend tests read the outbox
   directly. Do the same — a Playwright helper that runs
   `SELECT body_text FROM email_outbox WHERE recipient = $1 ORDER BY created_at DESC LIMIT 1`
   through `pg` and extracts the six digits. Requires no mail provider and no
   dev-only endpoint. `CM_RESEND_API_KEY` unset is fine locally; `config.rs:714`
   only warns.
3. **One compose file** for the local stack: PostGIS (already `cm-test-pg`),
   `firebase emulators:start --only auth`, the API with
   `FIREBASE_AUTH_EMULATOR_HOST`, Next on `PORT=3100` (never 3000 on this
   machine — see the local-checks note). CI runs the same file.

Acceptance: a Playwright test can complete "sign up as contractor with
Google" against the local stack with no code stubbed.

### Phase 3 · Browser journeys — done for email + Google-guard; Google popup written, unrun

Landed 2026-09-16: `cm-frontend/tests/e2e/auth.spec.ts` and `claim.spec.ts`.
**8 passed, 2 skipped, 23 s** on the local stack. The 63 cells stay
API-level; the browser proves only what the browser adds. Two design facts
shaped the files: registration is 10/hour per address and the suite is one
address, so the run is budgeted at nine registrations and journeys were
merged to fit (the homeowner bounce rides on journey 1; owner-redirect and
already-claimed share one test); and Next's dev overlay is `role="alert"`
and sits over the shell footer, so alerts are scoped to the `form` and
"Sign out" is activated by keyboard.

| # | Journey | Edges | Status |
|---|---|---|---|
| 1 | Homeowner signs up with email → code → `/app` *"Hiring someone"* → `account_type` homeowner → device cookie set → bounced from `/app/claim` → no CTA on a listing → sign out → sign in again with no code | A1.4, B.1, B.8, H.3, H.4, H.10, H.8, B.14, B.3 | **green** |
| 2 | Contractor signs up with email → `/app` *"Claim your listing"* / *"Find your listing"* → `account_type` contractor | A1.5, H.4 | **green** |
| 3 | Unknown address and wrong password: same banner, same 401; eighth failure locks; correct password then refused | A1.1, B.4, B.5, B.6 | **green** |
| 4 | Duplicate address on `/signup` → field error under email; the existing account is still its original side | A2.15, H.6 | **green** |
| 5 | `/signup` with no side chosen: Google button sends no request, shows the choose-a-side banner, a radio clears it | H.5 | **green** |
| 6 | `/login` Google for an unknown identity → FED-400 banner; `/signup` as contractor with the same identity → `/app`, contractor | A1.2, A1.7 | *skipped*: needs the Auth emulator (Java) |
| 7 | Existing Google homeowner, `/signup` with the contractor radio → `/app` as **homeowner** | A1.6, A3.7 | *skipped*: same |
| 8 | Remembered browser | B.3, B.14 | folded into 1 |
| 9 | Signed-out visitor on `/pros/slug` → *"Is this your business?"* → `/signup?next=` → contractor + email → code → **claim form for that listing** → submit → `/app` *"Your listing"* → `/app/listing` → CTA gone | H.7, A1.5, B.8, H.4, H.11, G.3, H.8 | **green** |

The original plan, for reference:

| # | Journey | Edges |
|---|---|---|
| 1 | Homeowner signs up with email → code screen → code from outbox → `/app` shows *"Hiring someone"* → `POST /v1/jobs` 201 → claims 403 | A1.4, B.1, B.8, H.3, H.4 |
| 2 | Contractor signs up with email → code → `/app` shows *"Claim your listing"* → `POST /v1/jobs` 403 | A1.5, H.4 |
| 3 | Unknown address and wrong password produce the identical banner; the 8th wrong attempt locks and the right password is then refused | A1.1, B.4, B.5, B.6, H.4 |
| 4 | Duplicate email on `/signup` shows the field error under the email input; the existing account still signs in and is still its original side | A2.5, A2.6, H.6 |
| 5 | On `/signup` with no radio chosen, the Google button sends no request and shows the choose-a-side banner; choosing a radio clears it | H.5 |
| 6 | On `/login`, Google for an unknown account shows FED-400 in the banner; the same identity on `/signup` as contractor creates the account and lands on `/app` | A1.2, A1.7, H.4 |
| 7 | Existing Google homeowner, `/signup` with the contractor radio + Google → lands on `/app` as **homeowner**, claims 403 | A3.7 |
| 8 | Remembered browser: second email sign-in skips the code; logout keeps the device cookie; the next sign-in still skips it | B.3, B.14 |
| 9 | Signed-out visitor on `/pros/slug` → *"Is this your business?"* → `/signup?next=…` → contractor + email → code → lands on the **claim form for that listing** → submit → `/app` shows *"Your listing"* → `/app/listing` renders → the public page no longer shows the CTA | H.7, A1.5, B.8, H.4, H.11, G.3, H.8 |

Journey 9 is the whole product in one test and should be the one that runs
first in CI.

Acceptance: nine green against the Phase 2 stack, in CI, with traces on
first retry (already configured).

### Phase 4 · Claiming edge cases in the browser — done

| # | Journey | Edges | Status |
|---|---|---|---|
| 10 | Homeowner visits `/app/claim?contractor=…` → bounced to `/app`; the public listing shows no CTA | H.10, H.8 | **green** (inside journey 1) |
| 11 | Two contractor sessions submit the same listing; statuses are exactly `[201, 409]`; the winner sees *"Your listing"*, the loser the API's copy in the form alert | G.8, H.11 | **green** |
| 12 | A contractor who already owns a listing opens another's claim link → `/app/listing` | H.10, G.5 | **green** |
| 13 | "Already claimed" renders for a claimed slug with no form | H.10 | **green** (same test as 12) |

Fixture listings are drawn from four hash buckets so parallel tests never
race each other for a row; `scripts/e2e-seed.sh --fresh` restores them.

### Phase 5 · Production checklist (config, not code) — open

Not something this repository can tick. Recorded once, re-checked at every
deploy that touches auth:

- Firebase console: Google and Facebook providers enabled; **account linking
  off** (this is what strips the email claim and why the popup's copy is
  forwarded); authorised domains include `www.contractorsmarketplace.co`.
- Facebook app in **Live** mode with the `email` permission approved, or
  every Facebook sign-up arrives address-less.
- `CM_RESEND_API_KEY` and `CM_MAIL_FROM` set, or no code, reset or
  verification mail is ever sent (`config.rs:714`).
- `FIREBASE_AUTH_EMULATOR_HOST` unset (the config refuses to start otherwise).
- HTTPS end to end: every cookie is `__Host-` and will not be set over HTTP.
- `CM_SESSION_IDLE_DAYS` ≤ `CM_SESSION_ABSOLUTE_DAYS` (defaults 14 / 90).
- Back up before any migration touching `users`, `oauth_identities` or
  `contractor_claims` — see the deploy note.

### Accepted soft spots (design, not backlog)

- Facebook account with no address and lost Facebook access: support only.
- Two accounts, one human (different addresses across methods): linking
  prevents it going forward; nothing merges two existing accounts.
- No unlink, no account-side change, no self-service suspension. Each is a
  product decision recorded in the code; none is a gap this plan closes.

## Re-verify

```bash
# backend, all seven suites this document is built on
DATABASE_URL="postgres://postgres:postgres@localhost:55432/cm_test" \
  cargo test -p cm-api --test auth --test google --test login_codes \
    --test password_reset --test security --test claims --test email_verify

# browser: seed once, then the suite starts its own API (18081) and Next (13100)
cm-frontend/scripts/e2e-seed.sh
cd cm-frontend && E2E_LOCAL_STACK=1 npm run test:e2e
```
