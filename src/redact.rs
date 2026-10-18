//! Secret redaction: what low hides from the model and the session log.

/// Secret prefixes: the prefix, what to call it, the shortest body that counts, and the body's non-alphanumeric chars.
/// The formats and lengths follow gitleaks' rules.
const SECRETS: &[(&str, &str, usize, &str)] = &[
    ("AKIA", "aws key", 16, ""),
    ("ASIA", "aws key", 16, ""),
    ("AIza", "google api key", 35, "_-"),
    ("ghp_", "github token", 36, ""),
    ("gho_", "github token", 36, ""),
    ("ghs_", "github token", 36, ""),
    ("ghu_", "github token", 36, ""),
    ("ghr_", "github token", 36, ""),
    ("github_pat_", "github token", 22, "_"),
    ("glpat-", "gitlab token", 20, "_-"),
    ("sk-", "api key", 20, "_-"),
    ("sk_live_", "stripe key", 24, ""),
    ("sk_test_", "stripe key", 24, ""),
    ("rk_live_", "stripe key", 24, ""),
    ("rk_test_", "stripe key", 24, ""),
    ("oc_", "opencode key", 20, "_"),
    ("gsk_", "groq key", 52, ""),
    ("xai-", "xai key", 80, "_"),
    ("hf_", "hugging face token", 34, ""),
    ("npm_", "npm token", 36, ""),
    ("pypi-AgEIcHlwaS5vcmc", "pypi token", 50, "_-"),
    ("cio", "crates.io token", 30, ""),
    ("dckr_pat_", "docker token", 27, "_-"),
    ("hvs.", "vault token", 90, "_-"),
    ("SG.", "sendgrid key", 66, "._-"),
    ("shpat_", "shopify token", 32, ""),
    ("shpss_", "shopify token", 32, ""),
    ("dop_v1_", "digitalocean token", 64, ""),
    ("lin_api_", "linear key", 40, ""),
    ("sbp_", "supabase token", 40, ""),
    ("xoxb-", "slack token", 10, "-"),
    ("xoxp-", "slack token", 10, "-"),
    ("xoxa-", "slack token", 10, "-"),
    ("xoxs-", "slack token", 10, "-"),
    ("xoxe-", "slack token", 10, "-"),
    ("xapp-", "slack token", 10, "-"),
];
/// Prefixes that also start words and names, so their body must hold a digit to count.
const WORDY: &[&str] = &["sk-", "oc_", "sk_live_", "sk_test_", "rk_live_", "rk_test_"];
/// The bytes a secret can start with: the table's prefixes, '-' for a PEM block and 'e' for a JSON web token.
const STARTS: [bool; 256] = {
    let mut starts = [false; 256];
    starts[b'-' as usize] = true;
    starts[b'e' as usize] = true;
    let mut i = 0;
    while i < SECRETS.len() {
        starts[SECRETS[i].0.as_bytes()[0] as usize] = true;
        i += 1;
    }
    starts
};
/// Written-out escapes a secret can follow in JSON or a URL, where the byte before it is a letter or digit.
const ESCAPES: &[&str] = &["\\n", "\\t", "\\r", "%20", "%0A", "%0a", "%0D", "%0d", "%09"];

pub fn redact(text: &str) -> (String, usize) {
    let (mut out, mut hidden, mut copied, mut i) = (String::new(), 0, 0, 0);
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let boundary = i == 0
            || !(bytes[i - 1].is_ascii_alphanumeric() || matches!(bytes[i - 1], b'_' | b'-'))
            || ESCAPES.iter().any(|e| text[..i].ends_with(e));
        if boundary
            && STARTS[bytes[i] as usize]
            && let Some((len, kind)) = secret(&text[i..])
        {
            out += &text[copied..i];
            out += &format!("[redacted {kind}]");
            (hidden, i) = (hidden + 1, i + len);
            copied = i;
            continue;
        }
        i += 1;
    }
    if hidden == 0 {
        return (text.into(), 0);
    }
    out += &text[copied..];
    (out, hidden)
}

fn secret(s: &str) -> Option<(usize, &'static str)> {
    // The header line can end in a newline or, inside JSON such as a cloud service account file, in a written-out \n.
    if let Some(rest) = s.strip_prefix("-----BEGIN ")
        && let Some(head) = rest.find("-----").filter(|&h| h < 64 && !rest[..h].contains('\n'))
        && rest[..head].contains("PRIVATE KEY")
    {
        let end = s.find("-----END ").and_then(|e| Some(e + 9 + s[e + 9..].find("-----")? + 5)).unwrap_or(s.len());
        return Some((end, "private key"));
    }
    // A JSON web token: three base64url parts, of which the first two are JSON objects ("eyJ" encodes `{"`).
    if s.starts_with("eyJ") {
        let len = s.find(|c: char| !(c.is_ascii_alphanumeric() || "_-.".contains(c))).unwrap_or(s.len());
        if let [head, claims, _] = s[..len].split('.').collect::<Vec<_>>()[..]
            && head.len() >= 10
            && claims.len() >= 10
            && claims.starts_with("eyJ")
        {
            return Some((len, "json web token"));
        }
    }
    SECRETS.iter().find_map(|&(prefix, kind, min, extra)| {
        let body = s.strip_prefix(prefix)?;
        let upper = matches!(prefix, "AKIA" | "ASIA");
        let allowed = |c: char| c.is_ascii_alphanumeric() && !(upper && c.is_ascii_lowercase()) || extra.contains(c);
        let len = body.find(|c| !allowed(c)).unwrap_or(body.len());
        let keyish = !WORDY.contains(&prefix) || body[..len].bytes().any(|b| b.is_ascii_digit());
        (len >= min && keyish).then_some((prefix.len() + len, kind))
    })
}
