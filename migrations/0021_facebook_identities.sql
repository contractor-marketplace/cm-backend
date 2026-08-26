-- 0021 · Facebook as a second federated provider.
--
-- This is a one-line change because the rule that matters was never
-- provider-specific: identities resolve on (provider, subject) and never on
-- email address. A second provider adds a second namespace of subjects, not a
-- second way to find an account. Both unique constraints already say so —
-- (provider, subject) is global, (user_id, provider) allows one identity per
-- provider per account, so an account may hold one Google and one Facebook.
--
-- For Facebook the subject stored is the app-scoped user id Meta returns, taken
-- from the verified token's `firebase.identities`, exactly as the Google
-- subject is. It is stable only for as long as the Meta App ID is: sign-in
-- through a different Meta app returns different ids for the same people and
-- would orphan every row here. The App ID is therefore not a value to change
-- casually, and dropping Firebase later re-links nobody only if it is kept.

ALTER TABLE oauth_identities
    DROP CONSTRAINT oauth_identities_provider_check;

ALTER TABLE oauth_identities
    ADD CONSTRAINT oauth_identities_provider_check
    CHECK (provider IN ('google', 'facebook'));
