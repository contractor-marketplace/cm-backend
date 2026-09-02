//! The emails this product sends, rendered.
//!
//! Every message carries **both** a plain-text and an HTML body. The text part
//! is not a fallback nobody reads: a message with no text alternative scores
//! worse with spam filters, and it is what a screen reader, a watch, and a
//! terminal client show. The HTML part is what almost everybody actually sees.
//!
//! Rendering lives here — beside the flows that issue the values — rather than
//! in the worker, which only posts finished bodies. That is what lets a body
//! contain a sign-in code or a single-use link at all: those exist only inside
//! the transaction that issues them.
//!
//! ## Writing HTML for email
//!
//! Email clients are not browsers, and the rules below are not preferences:
//!
//! * **Tables for layout.** Outlook renders through Word, which has no flexbox
//!   and no grid. A table with explicit widths is the only layout that lands
//!   the same way everywhere.
//! * **Inline styles.** Gmail strips `<style>` blocks in several contexts —
//!   notably when it clips a long message — so a stylesheet is a design that
//!   sometimes vanishes.
//! * **No external images.** Most clients block remote images by default, and
//!   a tracking pixel is exactly what a spam filter looks for. The wordmark is
//!   text, so the message looks finished before anything is downloaded, and
//!   there is nothing to consent to.
//! * **Escape everything interpolated.** Job titles and saved-search names are
//!   typed by users and land inside markup. `escape` below is not decoration.

/// A rendered message: what to put in the three outbox columns.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/* ── the shared shell ──────────────────────────────────────────────────────
 *
 * One document wrapper, three messages. Written once because the alternative
 * is three near-identical HTML documents drifting apart the first time a
 * colour changes.
 */

/* The front end's tokens, transcribed. Kept as constants with the same names
 * the stylesheet uses (`app/globals.css`, `components/ui/styles.ts`) so the two
 * can be compared line by line when either moves. */

/// Ground, ink and the single accent.
const PAPER: &str = "#ffffff";
const SUNKEN: &str = "#f2f5f9";
const INK: &str = "#1c2739";
const INK_MID: &str = "#4c5a75";
const INK_SOFT: &str = "#6b7688";
const RULE: &str = "#dfe4ec";
const EMBER: &str = "#cf4b00";

/* Type. The site sets display in Barlow Condensed, body in Barlow and labels
 * in Azeret Mono. Named here with real fallbacks rather than assumed: Gmail
 * and Outlook load no webfont at all, so every stack has to degrade to
 * something with the same proportions — a narrow grotesque for the condensed
 * face, a plain one for the body. */
const FONT_DISPLAY: &str = "'Barlow Condensed','Arial Narrow',Arial,sans-serif";
const FONT_BODY: &str = "'Barlow','Helvetica Neue',Helvetica,Arial,sans-serif";
const FONT_MONO: &str = "'Azeret Mono',ui-monospace,'SF Mono',Menlo,Consolas,monospace";

/* Shape. "Everything curves. The scale is generous rather than timid — a 4px
 * radius on a 200px card reads as a rendering artefact, not a decision." */
/// `--radius-2xl`, the sheet's radius.
const RADIUS_SHEET: &str = "32px";
/// `--radius-lg`, for the blocks nested inside a sheet.
const RADIUS_INNER: &str = "20px";

/// Escape text for interpolation into markup.
///
/// The five characters that can end an attribute, open a tag, or terminate an
/// entity. Applied to every value that came from a person — a job title, a
/// saved-search name — because those reach this module verbatim.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Wrap a message body in the shared document.
///
/// `preheader` is the grey line an inbox shows next to the subject. Setting it
/// deliberately is the difference between "Your sign-in code expires in ten
/// minutes" and whatever the first words of the markup happen to be.
///
/// `body` is trusted markup built by the callers below; anything they
/// interpolate from a person has already been through `escape`.
pub fn shell(preheader: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="x-apple-disable-message-reformatting">
<meta name="color-scheme" content="light">
<meta name="supported-color-schemes" content="light">
<title>ContractorMarketplace</title>
<!-- Apple Mail and a few others honour this; Gmail and Outlook ignore it and
     fall back down each stack. Nothing depends on it loading. -->
<link href="https://fonts.googleapis.com/css2?family=Barlow:wght@400;500;600&family=Barlow+Condensed:wght@600;700&family=Azeret+Mono:wght@400;500&display=swap" rel="stylesheet">
</head>
<body style="margin:0;padding:0;background-color:{SUNKEN};font-family:{FONT_BODY};-webkit-text-size-adjust:100%;">
<!-- Shown in the inbox list beside the subject, and nowhere else. -->
<div style="display:none;max-height:0;overflow:hidden;opacity:0;">{preheader}</div>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="background-color:{SUNKEN};">
<tr><td align="center" style="padding:40px 12px;">

<!-- The sheet: a ruled region, not a card. Border on all sides, no shadow. -->
<table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0" style="width:100%;max-width:600px;background-color:{PAPER};border:1px solid {RULE};border-radius:{RADIUS_SHEET};">

<tr><td style="padding:32px 36px 0 36px;">
<!-- One word, one colour, as the wordmark is set on the site. -->
<span style="font-family:{FONT_DISPLAY};font-size:19px;font-weight:700;letter-spacing:-0.01em;color:{INK};">ContractorMarketplace</span>
</td></tr>

<tr><td style="padding:22px 36px 36px 36px;">{body}</td></tr>

</table>

<table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0" style="width:100%;max-width:600px;">
<tr><td style="padding:20px 36px;font-family:{FONT_MONO};font-size:11px;line-height:18px;letter-spacing:0.11em;text-transform:uppercase;color:{INK_SOFT};">
Licensed contractors in Los Angeles County
</td></tr>
</table>

</td></tr>
</table>
</body>
</html>"#
    )
}

/// Display type: the condensed face, set tight, as `.u-display` sets it.
pub fn heading(text: &str) -> String {
    format!(
        r#"<h1 style="margin:0 0 14px 0;font-family:{FONT_DISPLAY};font-size:34px;line-height:0.95;font-weight:700;letter-spacing:-0.005em;color:{INK};">{text}</h1>"#
    )
}

/// The small structural label: mono, uppercase, widely tracked. Rationed on
/// the site to section headers, and rationed here to one per message.
pub fn label(text: &str) -> String {
    format!(
        r#"<div style="margin:0 0 10px 0;font-family:{FONT_MONO};font-size:11px;line-height:1.2;letter-spacing:0.11em;text-transform:uppercase;color:{INK_SOFT};">{text}</div>"#
    )
}

/// A paragraph of body copy.
pub fn paragraph(html: &str) -> String {
    format!(
        r#"<p style="margin:0 0 14px 0;font-family:{FONT_BODY};font-size:15px;line-height:23px;color:{INK_MID};">{html}</p>"#
    )
}

/// A primary action.
///
/// Midnight, not ember: `primary` on the site is `bg-ink text-panel`, and
/// ember is the *hover*. And fully round — "a pill is the shape a button wants
/// when the rest of the system curves; a large-but-finite radius on a short
/// wide element just looks like a rectangle that lost its nerve."
///
/// A padded table cell rather than a styled `<a>`: Outlook ignores padding on
/// an inline element, which turns a button into bare underlined text.
pub fn button(url: &str, label: &str) -> String {
    format!(
        r#"<table role="presentation" cellpadding="0" cellspacing="0" border="0" style="margin:22px 0;">
<tr><td style="background-color:{INK};border-radius:999px;">
<a href="{url}" style="display:inline-block;padding:14px 28px;font-family:{FONT_BODY};font-size:15px;font-weight:500;color:{PAPER};text-decoration:none;">{label}</a>
</td></tr></table>"#
    )
}

/// An inline link in body copy.
///
/// Ember, which is where the accent belongs: on the site it is "the single
/// accent", reserved for the lines meant to be seen rather than spent as a
/// fill. `label` is escaped; `url` is built by us.
pub fn link(url: &str, label: &str) -> String {
    format!(
        r#"<a href="{url}" style="color:{EMBER};text-decoration:none;font-weight:500;">{label}</a>"#,
        label = escape(label),
    )
}

/// Small print under a rule: expiry notes, "if this wasn't you", unsubscribes.
pub fn footnote(html: &str) -> String {
    format!(
        r#"<div style="margin-top:26px;padding-top:20px;border-top:1px solid {RULE};font-family:{FONT_BODY};font-size:13px;line-height:20px;color:{INK_SOFT};">{html}</div>"#
    )
}

/* ── the messages ──────────────────────────────────────────────────────── */

/// The sign-in code.
///
/// The code is set large and letter-spaced because it is transcribed by hand,
/// usually from a phone held next to a laptop, and it appears in the subject
/// line as well so it can be read from the inbox list without opening
/// anything.
pub fn login_code(code: &str) -> Rendered {
    let body = format!(
        "{label}{heading}{lead}\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:8px 0 4px 0;\">\
<tr><td style=\"background-color:{SUNKEN};border:1px solid {RULE};border-radius:{RADIUS_INNER};padding:20px 30px;\
font-family:{FONT_MONO};font-size:30px;line-height:36px;font-weight:500;letter-spacing:0.24em;\
font-variant-numeric:tabular-nums;color:{INK};\">{code}</td></tr>\
</table>{note}",
        label = label("Signing in"),
        heading = heading("Your sign-in code"),
        lead = paragraph("Enter this code to finish signing in."),
        note = footnote(
            "This code expires in 10 minutes and can be used once. If you did not try \
             to sign in, you can ignore this email — the code is useless without your \
             password."
        ),
    );

    Rendered {
        subject: format!("{code} is your sign-in code"),
        text: format!(
            "Your sign-in code is:\n\n    {code}\n\n\
             It expires in 10 minutes. If you did not try to sign in, you can \
             ignore this email — nobody can use the code without your password.",
        ),
        html: shell(&format!("{code} — expires in 10 minutes"), &body),
    }
}

/// The code proving control of an added or changed address.
///
/// Distinct from the sign-in code on purpose: "your sign-in code" on an email
/// that is not about signing in reads as somebody else trying to get in.
pub fn email_verify(code: &str) -> Rendered {
    let body = format!(
        "{label}{heading}{lead}\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:8px 0 4px 0;\">\
<tr><td style=\"background-color:{SUNKEN};border:1px solid {RULE};border-radius:{RADIUS_INNER};padding:20px 30px;\
font-family:{FONT_MONO};font-size:30px;line-height:36px;font-weight:500;letter-spacing:0.24em;\
font-variant-numeric:tabular-nums;color:{INK};\">{code}</td></tr>\
</table>{note}",
        label = label("Account"),
        heading = heading("Confirm this email address"),
        lead = paragraph(
            "Enter this code on your account page to confirm the address and \
             turn on email notifications."
        ),
        note = footnote(
            "This code expires in 10 minutes and can be used once. If you did not \
             add this address to an account, you can ignore this email — nothing \
             will be sent here again."
        ),
    );

    Rendered {
        subject: format!("{code} confirms your email address"),
        text: format!(
            "Your confirmation code is:\n\n    {code}\n\n\
             Enter it on your account page to confirm this address. It expires \
             in 10 minutes. If you did not add this address to an account, you \
             can ignore this email.",
        ),
        html: shell(&format!("{code} — confirms this address"), &body),
    }
}

/// The password-reset link. `link` is the full URL on the site.
pub fn password_reset(link: &str) -> Rendered {
    let body = format!(
        "{label}{heading}{lead}{button}{note}",
        label = label("Account"),
        heading = heading("Reset your password"),
        lead = paragraph(
            "Someone asked to reset the password for this account. If it was you, \
             choose a new one within the hour."
        ),
        button = button(link, "Choose a new password"),
        note = footnote(&format!(
            "This link works once and expires in an hour. If it was not you, ignore \
             this email — nothing has changed.<br><br>\
             If the button does not work, paste this into your browser:<br>\
             <span style=\"color:{INK_MID};word-break:break-all;\">{link}</span>"
        )),
    );

    Rendered {
        subject: "Reset your password".to_owned(),
        text: format!(
            "Someone asked to reset the password for this account. If it was \
             you, open this link within the hour:\n\n    {link}\n\n\
             If it was not you, ignore this email; the link works only once \
             and nothing has changed.",
        ),
        html: shell("Choose a new password — the link expires in an hour", &body),
    }
}

/* ── pieces the job digest is built from ───────────────────────────────────
 *
 * The digest itself is assembled in cm-domain, which is where the jobs and the
 * saved searches are. These are the parts it needs so that every message in
 * the product looks like the same product.
 */

/// One job in the weekly digest.
///
/// `title` is escaped here rather than at the call site: it is the one field a
/// stranger typed, and the escaping should live next to the markup it protects.
pub fn digest_job(url: &str, title: &str, facts: &str) -> String {
    format!(
        r#"<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="margin:0 0 12px 0;background-color:{PAPER};border:1px solid {RULE};border-radius:{RADIUS_INNER};">
<tr><td style="padding:18px 20px;">
<a href="{url}" style="font-family:{FONT_BODY};font-size:16px;line-height:22px;font-weight:600;color:{INK};text-decoration:none;">{title}</a>
<div style="margin-top:6px;font-family:{FONT_MONO};font-size:11px;line-height:1.5;letter-spacing:0.11em;text-transform:uppercase;color:{INK_SOFT};">{facts}</div>
</td></tr></table>"#,
        title = escape(title),
        facts = escape(facts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_in_the_subject_and_both_bodies() {
        let mail = login_code("123456");
        assert!(mail.subject.contains("123456"), "{}", mail.subject);
        assert!(mail.text.contains("123456"), "{}", mail.text);
        assert!(mail.html.contains("123456"), "{}", mail.html);
    }

    #[test]
    fn the_reset_link_survives_both_bodies_verbatim() {
        let link = "https://app.example.com/reset-password?token=abc-123_XYZ";
        let mail = password_reset(link);
        assert!(mail.text.contains(link), "{}", mail.text);
        assert!(
            mail.html.contains(&format!("href=\"{link}\"")),
            "the button must point at the real link: {}",
            mail.html
        );
        // Also spelled out, for a client that will not follow a button.
        assert!(mail.html.matches(link).count() >= 2, "{}", mail.html);
    }

    /// Every message is multipart. A text part is what a filter, a watch and a
    /// screen reader read, and dropping it would cost deliverability for a
    /// layout nobody in those places sees.
    #[test]
    fn every_message_carries_both_bodies() {
        for mail in [login_code("000000"), password_reset("https://x.test/r")] {
            assert!(!mail.text.trim().is_empty());
            assert!(mail.html.starts_with("<!doctype html>"), "{}", mail.html);
            assert!(!mail.subject.trim().is_empty());
        }
    }

    /// The mail has to look like the product, and it is the one surface where
    /// nobody notices it stopped: these are transcribed from
    /// `cm-frontend/app/globals.css` and `components/ui/styles.ts`, so a
    /// redesign there fails here rather than shipping a stale-looking inbox.
    #[test]
    fn the_templates_use_the_front_ends_type_shape_and_colour() {
        let html = login_code("123456").html;

        // Type: the three real faces, never a system stack.
        assert!(html.contains("'Barlow Condensed'"), "display face");
        assert!(html.contains("'Barlow',"), "body face");
        assert!(html.contains("'Azeret Mono'"), "mono face");

        // Shape: the sheet's radius, and no shadow anywhere — "a ruled region,
        // not a card … no elevation theater".
        assert!(html.contains("border-radius:32px"), "the sheet radius");
        assert!(!html.to_lowercase().contains("box-shadow"), "{html}");

        // Colour: midnight ink, mist rules, and the ground.
        assert!(html.contains("#1c2739"), "ink");
        assert!(html.contains("#dfe4ec"), "rule");

        // The wordmark is one word in one colour.
        assert!(html.contains(">ContractorMarketplace<"), "{html}");

        // Display type is set tight, as .u-display sets it.
        assert!(html.contains("line-height:0.95"), "{html}");
    }

    /// The button is midnight and fully round. `primary` on the site is
    /// `bg-ink text-panel` with `rounded-full`; ember is its *hover*, and a
    /// finite radius "looks like a rectangle that lost its nerve".
    #[test]
    fn the_primary_button_is_a_midnight_pill() {
        let html = password_reset("https://x.test/r").html;
        assert!(
            html.contains("background-color:#1c2739;border-radius:999px"),
            "{html}"
        );
        assert!(
            !html.contains("background-color:#cf4b00"),
            "ember is the hover, not the resting state: {html}"
        );
    }

    /// Labels are the mono face, uppercase and widely tracked.
    #[test]
    fn labels_are_set_the_way_the_site_sets_them() {
        let rendered = label("Signing in");
        assert!(rendered.contains("text-transform:uppercase"), "{rendered}");
        assert!(rendered.contains("letter-spacing:0.11em"), "{rendered}");
        assert!(rendered.contains("font-size:11px"), "{rendered}");
        assert!(rendered.contains("'Azeret Mono'"), "{rendered}");
    }

    /// The one that matters: a job title is typed by a stranger and lands
    /// inside markup. Unescaped, a title could close the anchor and open
    /// anything it liked in somebody else's inbox.
    #[test]
    fn a_hostile_job_title_cannot_break_out_of_the_markup() {
        let hostile = r#"</a><script>alert('x')</script><a href="https://evil.test">"#;
        let rendered = digest_job("https://x.test/jobs/1", hostile, "Plumbing, ZIP 90026");

        assert!(!rendered.contains("<script>"), "{rendered}");
        assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
        // The attacker's URL may appear as *text* — escaped, it renders as
        // characters and links nowhere. What must not appear is a real
        // attribute carrying it.
        assert!(
            !rendered.contains(r#"href="https://evil.test""#),
            "{rendered}"
        );
        assert!(
            rendered.contains("href=&quot;https://evil.test&quot;"),
            "{rendered}"
        );
        // Exactly the anchor this function opened, and no other.
        assert_eq!(rendered.matches("<a href=").count(), 1, "{rendered}");
    }

    #[test]
    fn escaping_covers_every_character_that_can_end_a_tag_or_an_attribute() {
        assert_eq!(
            escape(r#"<a href="x" title='y'>&</a>"#),
            "&lt;a href=&quot;x&quot; title=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(escape("Ibarra & Daughters"), "Ibarra &amp; Daughters");
        // Ordinary text is left exactly alone.
        assert_eq!(escape("Rewire a 1920s duplex"), "Rewire a 1920s duplex");
    }

    /// A preheader that is not set shows the first words of the markup in the
    /// inbox list, which is how "View this email in your browser" became the
    /// most-read sentence in email.
    #[test]
    fn the_preheader_is_set_and_hidden() {
        let html = login_code("424242").html;
        assert!(html.contains("424242 — expires in 10 minutes"), "{html}");
        assert!(html.contains("display:none;max-height:0"), "{html}");
    }
}
