//! Record/replay for HTTP-backed adapters, on a local [`wiremock`] server.
//!
//! A [`Cassette`] is a JSON file of request patterns and the responses to
//! give, in order (the last one repeats). A [`ReplayServer`] serves it,
//! counts requests and writes, records requests the cassette does not
//! cover, and can simulate [`Fault`]s for the conformance suite.
//!
//! Recording: build interactions from responses captured with your own
//! client (or by hand), then [`Cassette::save`] them through a
//! [`Redactor`]. Saving runs the redaction rules, then a scan for things
//! that look identifying, and refuses to write while anything is found.
//! The scan is a backstop, not a guarantee: it catches common shapes of
//! identifying data, and nothing it does not know the shape of. Read what
//! you record. The scan never prints the values it finds, nor a name that
//! itself looks identifying (such a key is shown as `<key N>`, its
//! position in its object).
//!
//! It scans the request path, query parameter names and values, response
//! header names and values, JSON object keys, strings and numbers at any
//! depth, and text bodies, for:
//!
//! - runs of 10 or more digits (any Unicode digits), inside any string
//!   (`/sites/<digits>/…`, a fraction included) or as a JSON number; the
//!   fractional digits of a value that is a JSON
//!   decimal number as a whole (`19.1234567890`) are not counted;
//! - UUIDs (dashed, or 32 hex digits in a row), MAC addresses, e-mail
//!   addresses and tokens;
//! - IPv4 and IPv6 addresses, except loopback, the unspecified address and
//!   the documentation ranges (192.0.2.0/24, 198.51.100.0/24,
//!   203.0.113.0/24, 2001:db8::/32);
//! - host names under host-like keys and query parameters (`host`,
//!   `hostname`, `*_host`, `*Host`, `server`, `fqdn`), except `localhost`,
//!   `*.localhost`, `*.example`, `example.com`, `example.org`,
//!   `example.net` (and their subdomains), `*.invalid` and `*.test`;
//! - any value other than an obvious placeholder (`0`, `00A0000000`,
//!   `REDACTED`, …) under identifying keys: `id`, `*_id`, `*Id`, `uuid`,
//!   `guid`, `serial*`, `mac`, `ssid`, e-mail, address, name and account
//!   keys;
//! - credentials: any value but `REDACTED` in a query parameter or header
//!   whose name suggests a secret (redaction replaces them);
//! - coordinates with more than two decimals.
//!
//! Redaction also replaces the values of per-request and trace id headers
//! (`*-request-id`, `*-trace-id`, `*-cf-id`, `cf-ray`; any case) with
//! `REDACTED`: they identify one exchange, so they are neither kept nor
//! refused. Their values are not held to the id-key rule, but to every
//! other check.
//!
//! Timestamps: an integer (or a string holding one) under a time-like key
//! (`time`, `timestamp`, `ts`, `date`, `epoch`, `since`, `until`,
//! `expires`, `created`, `updated`, `*_at`, `*At`, `*_time`, `*Time`, …)
//! that reads as seconds, milliseconds, microseconds or nanoseconds since
//! 1970 between the years 2000 and 2100 is not counted as a digit run.
//! Anywhere else an epoch timestamp is a finding.
//!
//! Versions: a dotted version string (`1.4.20231215100512`, `v2.0.1-rc.1`;
//! at least one dot, groups of at most 8 digits except one 12- or
//! 14-digit date-time build stamp from 2000 to 2049, an optional `v` and a
//! `-`/`+` suffix, and at most 12 digits in all outside the build stamp)
//! under a version key (`version`, `firmware`, `fw`, or a named version
//! such as `firmware_version`, `fwVersion`, `sw-version`, `appVersion`)
//! is not counted as a long digit run. Any other value under a version key
//! with ten or more digits in all, however split, is a finding; under
//! other keys the digit-run rule applies.
//!
//! A finding is resolved by redacting the value ([`Redactor::key`];
//! [`Redactor::literal`], which also applies to keys and names) or, for a
//! value checked and known not to identify anyone, by allowing it
//! explicitly: [`Redactor::allow`] (one location) or
//! [`Redactor::allow_key`] (a key or name, and every value under it).
//!
//! ```
//! use heimr_testkit::replay::{Cassette, Interaction, Redactor};
//! use serde_json::json;
//!
//! let mut c = Cassette::default();
//! c.push(Interaction::get("/v1/status").json(200, json!({ "serial": "SN-12345", "temperature": 20.5 })));
//! let r = Redactor::new().key("serial", json!("00A0000000"));
//! let clean = r.redact(&c);
//! assert!(r.scan(&clean).is_empty());
//! assert_eq!(clean.interactions[0].responses[0].json, Some(json!({ "serial": "00A0000000", "temperature": 20.5 })));
//! assert!(!r.scan(&c).is_empty(), "the unredacted serial is found");
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::driver::{Fault, Traffic};
use crate::upstream::Upstream;

/// What a request must look like to match an interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestPattern {
    /// HTTP method, e.g. `GET`.
    pub method: String,
    /// Exact path, e.g. `/v1/status`.
    pub path: String,
    /// Query parameters that must be present with these values (others
    /// are ignored).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
}

/// A response to give.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CannedResponse {
    /// HTTP status.
    pub status: u16,
    /// Headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// A JSON body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    /// A text body (used when `json` is absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// A request pattern and its responses, given in order; the last repeats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Interaction {
    /// What to match.
    pub request: RequestPattern,
    /// What to answer, in order.
    pub responses: Vec<CannedResponse>,
}

impl Interaction {
    /// An interaction for `method path` with no responses yet.
    pub fn new(method: &str, path: &str) -> Self {
        Self {
            request: RequestPattern {
                method: method.to_ascii_uppercase(),
                path: path.to_owned(),
                query: BTreeMap::new(),
            },
            responses: Vec::new(),
        }
    }

    /// `GET path`.
    pub fn get(path: &str) -> Self {
        Self::new("GET", path)
    }

    /// Also require query parameter `name=value`.
    #[must_use]
    pub fn query(mut self, name: &str, value: &str) -> Self {
        self.request.query.insert(name.to_owned(), value.to_owned());
        self
    }

    /// Answer with a JSON body.
    #[must_use]
    pub fn json(mut self, status: u16, body: Value) -> Self {
        self.responses.push(CannedResponse {
            status,
            headers: BTreeMap::new(),
            json: Some(body),
            body: None,
        });
        self
    }

    /// Answer with a text body.
    #[must_use]
    pub fn text(mut self, status: u16, body: &str) -> Self {
        self.responses.push(CannedResponse {
            status,
            headers: BTreeMap::new(),
            json: None,
            body: Some(body.to_owned()),
        });
        self
    }
}

/// A set of interactions; see the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cassette {
    /// Interactions, matched first to last.
    pub interactions: Vec<Interaction>,
}

/// Loading or saving a cassette failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum CassetteError {
    /// Reading or writing the file failed.
    Io(std::io::Error),
    /// The file is not a cassette.
    Json(serde_json::Error),
    /// Something identifying is still there after redaction.
    Identifying(Vec<Finding>),
}

impl fmt::Display for CassetteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cassette I/O: {e}"),
            Self::Json(e) => write!(f, "cassette JSON: {e}"),
            Self::Identifying(found) => {
                write!(
                    f,
                    "refusing to save: {} identifying values remain",
                    found.len()
                )?;
                for x in found {
                    write!(f, "\n  {x}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CassetteError {}

impl Cassette {
    /// Add an interaction.
    pub fn push(&mut self, i: Interaction) {
        self.interactions.push(i);
    }

    /// Parse a cassette from JSON text (e.g. `include_str!` of a fixture).
    pub fn parse(json: &str) -> Result<Self, CassetteError> {
        serde_json::from_str(json).map_err(CassetteError::Json)
    }

    /// Read a cassette file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CassetteError> {
        let text = std::fs::read_to_string(path).map_err(CassetteError::Io)?;
        serde_json::from_str(&text).map_err(CassetteError::Json)
    }

    /// Redact with `redactor`, scan, and write pretty JSON to `path` only
    /// if the scan finds nothing.
    pub fn save(&self, path: impl AsRef<Path>, redactor: &Redactor) -> Result<(), CassetteError> {
        let clean = redactor.redact(self);
        let found = redactor.scan(&clean);
        if !found.is_empty() {
            return Err(CassetteError::Identifying(found));
        }
        let mut text = serde_json::to_string_pretty(&clean).map_err(CassetteError::Json)?;
        text.push('\n');
        std::fs::write(path, text).map_err(CassetteError::Io)
    }
}

// --- server -----------------------------------------------------------------

/// A request the server received.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Received {
    /// Method.
    pub method: String,
    /// Path.
    pub path: String,
    /// Raw query string.
    pub query: String,
}

impl fmt::Display for Received {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.path)?;
        if !self.query.is_empty() {
            write!(f, "?{}", self.query)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct State {
    cassette: Cassette,
    calls: Mutex<Vec<usize>>,
    fault: Mutex<Option<Fault>>,
    requests: AtomicU64,
    writes: AtomicU64,
    garbage: AtomicU64,
    log: Mutex<Vec<Received>>,
    unmatched: Mutex<Vec<Received>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Swap types throughout a JSON value: numbers become strings, strings
/// become numbers, booleans become null.
fn type_swapped(v: &Value) -> Value {
    match v {
        Value::Number(n) => Value::String(n.to_string()),
        Value::String(_) => json!(-1),
        Value::Bool(_) => Value::Null,
        Value::Array(a) => Value::Array(a.iter().map(type_swapped).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), type_swapped(v)))
                .collect(),
        ),
        Value::Null => json!({}),
    }
}

fn canned(r: &CannedResponse) -> ResponseTemplate {
    let mut t = ResponseTemplate::new(r.status);
    for (k, v) in &r.headers {
        t = t.insert_header(k.as_str(), v.as_str());
    }
    match (&r.json, &r.body) {
        (Some(j), _) => t.set_body_json(j),
        (None, Some(b)) => t.set_body_string(b.clone()),
        (None, None) => t,
    }
}

impl State {
    fn find(&self, req: &Request) -> Option<usize> {
        let query: BTreeMap<String, String> = req
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        self.cassette.interactions.iter().position(|i| {
            i.request.method.eq_ignore_ascii_case(req.method.as_str())
                && i.request.path == req.url.path()
                && i.request
                    .query
                    .iter()
                    .all(|(k, v)| query.get(k).is_some_and(|q| q == v))
        })
    }

    fn garbage(&self, matched: Option<&CannedResponse>) -> ResponseTemplate {
        let n = self.garbage.fetch_add(1, Ordering::Relaxed);
        let json = |body: &str| {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body.to_owned())
        };
        match n % 6 {
            0 => json("{\"truncated\": [1, 2,"),
            1 => json("null"),
            2 => json("[]"),
            3 => ResponseTemplate::new(200).set_body_string("<html>gateway error</html>"),
            4 => json("{\"value\": 1e999999, \"nested\": {\"deep\": [[[[[[[[[[]]]]]]]]]]}}"),
            _ => match matched.and_then(|r| r.json.as_ref()) {
                Some(j) => ResponseTemplate::new(200).set_body_json(type_swapped(j)),
                None => json("{}"),
            },
        }
    }
}

struct Replayer(Arc<State>);

impl Respond for Replayer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let s = &self.0;
        s.requests.fetch_add(1, Ordering::Relaxed);
        let method = req.method.as_str().to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
            s.writes.fetch_add(1, Ordering::Relaxed);
        }
        let received = Received {
            method,
            path: req.url.path().to_owned(),
            query: req.url.query().unwrap_or_default().to_owned(),
        };
        lock(&s.log).push(received.clone());
        let matched = s.find(req).and_then(|i| {
            let mut calls = lock(&s.calls);
            let n = calls[i];
            calls[i] += 1;
            let rs = &s.cassette.interactions[i].responses;
            rs.get(n.min(rs.len().saturating_sub(1)))
        });
        match *lock(&s.fault) {
            Some(Fault::Unavailable) => {
                return ResponseTemplate::new(503).set_body_string("service unavailable");
            }
            Some(Fault::Rejected) => {
                return ResponseTemplate::new(401)
                    .set_body_json(json!({ "error": "invalid credentials" }));
            }
            Some(Fault::Malformed) => return s.garbage(matched),
            _ => {}
        }
        match matched {
            Some(r) => canned(r),
            None => {
                let msg = format!("no cassette entry for {received}");
                lock(&s.unmatched).push(received);
                ResponseTemplate::new(404).set_body_string(msg)
            }
        }
    }
}

/// Serves a [`Cassette`] on a local port. See the [module docs](self).
#[derive(Debug)]
pub struct ReplayServer {
    server: MockServer,
    state: Arc<State>,
}

impl ReplayServer {
    /// Start serving `cassette`.
    pub async fn start(cassette: Cassette) -> Self {
        let state = Arc::new(State {
            calls: Mutex::new(vec![0; cassette.interactions.len()]),
            cassette,
            ..State::default()
        });
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(Replayer(state.clone()))
            .mount(&server)
            .await;
        Self { server, state }
    }

    /// Base URL, e.g. `http://127.0.0.1:41234`.
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Simulate `fault` from now on (`None` stops).
    pub fn set_fault(&self, fault: Option<Fault>) {
        *lock(&self.state.fault) = fault;
    }

    /// Requests and writes so far.
    pub fn traffic(&self) -> Traffic {
        Traffic::new(
            self.state.requests.load(Ordering::Relaxed),
            self.state.writes.load(Ordering::Relaxed),
        )
    }

    /// Every request received, in order.
    pub fn received(&self) -> Vec<Received> {
        lock(&self.state.log).clone()
    }

    /// Requests no interaction matched (answered 404).
    pub fn unmatched(&self) -> Vec<Received> {
        lock(&self.state.unmatched).clone()
    }

    /// Use this server as a conformance [`Upstream`], with `settings`
    /// building the instance settings from the base URL.
    pub fn upstream(
        self,
        settings: impl Fn(&str) -> Value + Send + Sync + 'static,
    ) -> ReplayUpstream {
        ReplayUpstream {
            server: self,
            settings: Box::new(settings),
        }
    }
}

/// A [`ReplayServer`] as a conformance [`Upstream`]: supports every
/// [`Fault`] and counts traffic.
pub struct ReplayUpstream {
    server: ReplayServer,
    settings: Box<dyn Fn(&str) -> Value + Send + Sync>,
}

impl fmt::Debug for ReplayUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplayUpstream")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl ReplayUpstream {
    /// The server.
    pub fn server(&self) -> &ReplayServer {
        &self.server
    }
}

impl Upstream for ReplayUpstream {
    fn settings(&self) -> Value {
        (self.settings)(&self.server.uri())
    }

    fn inject(&self, fault: Option<Fault>) -> bool {
        self.server.set_fault(fault);
        true
    }

    fn traffic(&self) -> Option<Traffic> {
        Some(self.server.traffic())
    }
}

// --- redaction --------------------------------------------------------------

/// Something in a cassette that looks identifying.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Finding {
    /// Where, e.g. `interactions[0].responses[0].json.device.serial` for
    /// a value. A finding in a JSON key, query parameter name or header
    /// name ends in `#name`, and a key or name that itself looks
    /// identifying is shown as `<key N>`, its position among its
    /// siblings: `interactions[0].request.query.<key 0>#name`.
    pub location: String,
    /// What it looks like (never the value itself).
    pub what: &'static str,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: looks like {}", self.location, self.what)
    }
}

/// Redaction rules plus the identifying-value scan. See the [module
/// docs](self).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    keys: Vec<(String, Value)>,
    literals: Vec<(String, String)>,
    allowed: Vec<String>,
    allowed_keys: Vec<String>,
}

const REDACTED: &str = "REDACTED";

/// Header and query names whose values are always replaced.
fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "auth",
        "token",
        "key",
        "secret",
        "password",
        "cookie",
        "session",
        "signature",
    ]
    .iter()
    .any(|w| n.contains(w))
}

/// Whether `key` names an identifier: `id`, `ids`, `*_id(s)`, `*-id(s)`,
/// or camel case `*Id(s)`/`*ID(s)`.
fn id_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let camel = ["Id", "ID", "Ids", "IDs"].iter().any(|suffix| {
        key.strip_suffix(suffix)
            .and_then(|rest| rest.chars().last())
            .is_some_and(|c| c.is_ascii_alphanumeric())
    });
    matches!(k.as_str(), "id" | "ids")
        || ["_id", "_ids", "-id", "-ids"]
            .iter()
            .any(|s| k.ends_with(s))
        || camel
}

/// Whether a header names a per-request or trace id (`x-request-id`,
/// `x-amz-cf-id`, `x-trace-id`, `cf-ray`, …; any case). Redaction
/// replaces their values; the scan does not hold them to the id-key rule.
fn trace_header(name: &str) -> bool {
    let k = name.to_ascii_lowercase();
    k == "cf-ray"
        || k == "request-id"
        || k == "trace-id"
        || k.ends_with("-request-id")
        || k.ends_with("-trace-id")
        || k.ends_with("-cf-id")
}

/// JSON keys whose values identify a device, a person or a place.
fn identifying_key(key: &str) -> Option<&'static str> {
    let k = key.to_ascii_lowercase();
    let k = k.as_str();
    if k.contains("serial") {
        Some("a serial number")
    } else if k == "mac"
        || k.ends_with("_mac")
        || key.ends_with("Mac")
        || k.contains("mac_address")
        || k.contains("macaddress")
    {
        Some("a MAC address")
    } else if k.contains("uuid") || k.contains("guid") {
        Some("a UUID")
    } else if k.contains("ssid") {
        Some("a network name")
    } else if k.contains("email") {
        Some("an e-mail address")
    } else if matches!(k, "address" | "street" | "postcode" | "postal_code" | "zip") {
        Some("a postal address")
    } else if matches!(
        k,
        "name" | "first_name" | "last_name" | "full_name" | "owner"
    ) {
        Some("a personal name")
    } else if k.contains("account") {
        Some("an account number")
    } else if id_key(key) {
        Some("an identifier (an id key)")
    } else {
        None
    }
}

/// Whether `key` names a point in time (see the [module docs](self)).
fn time_like_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let camel = ["At", "Time", "Timestamp", "Date", "Ts"]
        .iter()
        .any(|suffix| {
            key.strip_suffix(suffix)
                .and_then(|rest| rest.chars().last())
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    matches!(
        k.as_str(),
        "t" | "ts"
            | "time"
            | "timestamp"
            | "date"
            | "datetime"
            | "epoch"
            | "since"
            | "until"
            | "expires"
            | "expiry"
            | "created"
            | "updated"
            | "modified"
    ) || [
        "_at",
        "-at",
        "_time",
        "-time",
        "_ts",
        "_date",
        "_epoch",
        "timestamp",
    ]
    .iter()
    .any(|s| k.ends_with(s))
        || camel
}

/// The integer digits of `s` if all of it is one decimal number (sign,
/// digits, an optional fraction and exponent).
fn integer_part(s: &str) -> Option<&str> {
    let t = s.trim();
    let t = t.strip_prefix(['-', '+']).unwrap_or(t);
    let int = t.split(['.', 'e', 'E']).next()?;
    let numeric = t.parse::<f64>().is_ok()
        && t.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'));
    (numeric && int.bytes().all(|b| b.is_ascii_digit())).then_some(int)
}

/// Seconds, milliseconds, microseconds or nanoseconds since 1970 between
/// 2000-01-01 and 2100-01-01.
fn plausible_epoch(s: &str) -> bool {
    const FROM: u128 = 946_684_800;
    const TO: u128 = 4_102_444_800;
    let Some(v) = integer_part(s).and_then(|i| i.parse::<u128>().ok()) else {
        return false;
    };
    [1, 1_000, 1_000_000, 1_000_000_000]
        .iter()
        .any(|scale| (FROM * scale..=TO * scale).contains(&v))
}

fn is_placeholder(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(_) => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => {
            s.is_empty()
                || s == REDACTED
                || s.contains("example")
                || s.chars()
                    .filter(char::is_ascii_alphanumeric)
                    .all(|c| c == '0' || (c.is_ascii_uppercase() && "AXZ".contains(c)))
        }
        _ => false,
    }
}

fn looks_like_email(s: &str) -> bool {
    s.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '@' && c != '.');
        match w.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.ends_with("example.com")
                    && !domain.ends_with("example.org")
                    && !domain.ends_with(".invalid")
                    && !domain.ends_with(".test")
            }
            None => false,
        }
    })
}

fn looks_like_mac(s: &str) -> bool {
    let b = s.as_bytes();
    let separated = b.windows(17).any(|w| {
        (0..17).all(|i| {
            if i % 3 == 2 {
                w[i] == b':' || w[i] == b'-'
            } else {
                w[i].is_ascii_hexdigit()
            }
        }) && !w.iter().all(|c| matches!(c, b'0' | b':' | b'-'))
    });
    // Three dot-separated groups of four, e.g. `0a1b.2c3d.4e5f`.
    let dotted = b.windows(14).any(|w| {
        (0..14).all(|i| {
            if i % 5 == 4 {
                w[i] == b'.'
            } else {
                w[i].is_ascii_hexdigit()
            }
        }) && !w.iter().all(|c| matches!(c, b'0' | b'.'))
    });
    separated || dotted
}

fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    let dashed = b.windows(36).any(|w| {
        (0..36).all(|i| {
            if matches!(i, 8 | 13 | 18 | 23) {
                w[i] == b'-'
            } else {
                w[i].is_ascii_hexdigit()
            }
        }) && !w.iter().all(|c| matches!(c, b'0' | b'-'))
    });
    // Exactly 32 hex digits in a row, with digits and letters.
    let bare = s.split(|c: char| !c.is_ascii_hexdigit()).any(|w| {
        w.len() == 32
            && w.bytes().all(|c| c.is_ascii_hexdigit())
            && w.bytes().any(|c| c.is_ascii_digit())
            && w.bytes().any(|c| c.is_ascii_alphabetic())
    });
    dashed || bare
}

/// An IPv4 address other than loopback, `0.0.0.0` or a documentation
/// range.
fn looks_like_ipv4(s: &str) -> bool {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter_map(|w| w.trim_matches('.').parse::<std::net::Ipv4Addr>().ok())
        .any(|ip| {
            let o = ip.octets();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || matches!(o, [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]))
        })
}

/// An IPv6 address other than `::1`, `::` or one in 2001:db8::/32.
fn looks_like_ipv6(s: &str) -> bool {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '.'))
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| w.matches(':').count() >= 2)
        .filter_map(|w| w.parse::<std::net::Ipv6Addr>().ok())
        .any(|ip| {
            let seg = ip.segments();
            !(ip.is_loopback() || ip.is_unspecified() || (seg[0] == 0x2001 && seg[1] == 0x0db8))
        })
}

/// Whether `key` names a host.
fn host_like_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let camel = ["Host", "Hostname"].iter().any(|suffix| {
        key.strip_suffix(suffix)
            .and_then(|rest| rest.chars().last())
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    matches!(k.as_str(), "host" | "hostname" | "server" | "fqdn")
        || ["_host", "-host", "_hostname", "-hostname"]
            .iter()
            .any(|s| k.ends_with(s))
        || camel
}

/// A host name (from a bare name or a URL) that is not reserved for
/// examples, tests or the local machine. IP addresses are left to the
/// address checks.
fn looks_like_hostname(s: &str) -> bool {
    let t = s.trim();
    let t = t.split_once("://").map_or(t, |(_, rest)| rest);
    let t = t.split(['/', '?', '#']).next().unwrap_or_default();
    let t = t.rsplit_once('@').map_or(t, |(_, host)| host);
    if t.starts_with('[') || t.parse::<std::net::Ipv6Addr>().is_ok() {
        return false;
    }
    let host = t
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.');
    let host = host.to_ascii_lowercase();
    if host.is_empty()
        || host == REDACTED.to_ascii_lowercase()
        || host.parse::<std::net::Ipv4Addr>().is_ok()
    {
        return false;
    }
    let reserved = |base: &str| host == base || host.ends_with(&format!(".{base}"));
    let exempt = [
        "localhost",
        "example",
        "example.com",
        "example.org",
        "example.net",
        "invalid",
        "test",
    ]
    .iter()
    .any(|b| reserved(b));
    let plausible = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
    plausible && !exempt && !is_placeholder(&Value::String(host.clone()))
}

fn looks_like_token(s: &str) -> bool {
    let jwt = s
        .split_whitespace()
        .any(|w| w.starts_with("eyJ") && w.matches('.').count() == 2);
    let bearer = s
        .to_ascii_lowercase()
        .split_once("bearer ")
        .is_some_and(|(_, rest)| {
            rest.trim() != REDACTED.to_ascii_lowercase() && rest.trim().len() >= 16
        });
    jwt || bearer
}

/// For a JSON number: a run of 10 or more digits, not all zeros, in its
/// integer part (its fraction is a measurement, not an identifier).
fn long_digits(s: &str) -> bool {
    digit_run(integer_part(s).unwrap_or(s))
}

/// A run of 10 or more digits, not all zeros, anywhere in `s`. Any
/// Unicode numeric character counts as a digit (full-width digits too).
fn digit_run(s: &str) -> bool {
    let mut run = 0usize;
    let mut zeros = true;
    for c in s.chars() {
        if c.is_numeric() {
            run += 1;
            zeros &= c == '0';
            if run >= 10 && !zeros {
                return true;
            }
        } else {
            run = 0;
            zeros = true;
        }
    }
    false
}

fn precise_coordinate(key: &str, v: &Value) -> bool {
    let k = key.to_ascii_lowercase();
    if !matches!(k.as_str(), "lat" | "latitude" | "lon" | "lng" | "longitude") {
        return false;
    }
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return false,
    };
    text.split_once('.').is_some_and(|(_, frac)| frac.len() > 2)
}

impl Redactor {
    /// No rules yet; the scan is always on.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the value of every JSON key named `key` (at any depth) with
    /// `placeholder`, e.g. `00A0000000` for a serial number.
    #[must_use]
    pub fn key(mut self, key: &str, placeholder: Value) -> Self {
        self.keys.push((key.to_owned(), placeholder));
        self
    }

    /// Replace every occurrence of `secret` in any string (paths, query
    /// values, headers, bodies) with `placeholder`.
    #[must_use]
    pub fn literal(mut self, secret: &str, placeholder: &str) -> Self {
        if !secret.is_empty() {
            self.literals
                .push((secret.to_owned(), placeholder.to_owned()));
        }
        self
    }

    /// Do not report findings at locations starting with `prefix`, as
    /// [`Finding::location`] spells them (for a value checked and known
    /// to be public, such as a published coordinate or a fixed resource
    /// number in a path): `.allow("interactions[0].request.path")`.
    #[must_use]
    pub fn allow(mut self, prefix: &str) -> Self {
        self.allowed.push(prefix.to_owned());
        self
    }

    /// Do not report findings for values directly under the JSON key or
    /// query parameter `key` (exact name, any depth; array elements
    /// included, nested objects not), for a key checked and known not to
    /// hold anything identifying, such as an `id` naming a metric.
    /// Credentials in query parameters are still reported.
    #[must_use]
    pub fn allow_key(mut self, key: &str) -> Self {
        self.allowed_keys.push(key.to_owned());
        self
    }

    fn key_allowed(&self, key: Option<&str>) -> bool {
        key.is_some_and(|k| self.allowed_keys.iter().any(|a| a == k))
    }

    fn text(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (secret, placeholder) in &self.literals {
            out = out.replace(secret.as_str(), placeholder);
        }
        out
    }

    fn json(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.json(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        let v = match self.keys.iter().find(|(key, _)| key == k) {
                            Some((_, placeholder)) => placeholder.clone(),
                            None => self.json(x),
                        };
                        (self.text(k), v)
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Values of secret-looking names become `REDACTED`; literal rules
    /// apply to names and values.
    fn names(&self, map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        map.iter()
            .map(|(k, v)| {
                let v = if secret_name(k) {
                    REDACTED.to_owned()
                } else {
                    self.text(v)
                };
                (self.text(k), v)
            })
            .collect()
    }

    /// A redacted copy: key and literal rules applied (literal rules also
    /// to JSON keys, query parameter names and header names),
    /// secret-looking headers and query parameters replaced.
    pub fn redact(&self, cassette: &Cassette) -> Cassette {
        let mut out = cassette.clone();
        for i in &mut out.interactions {
            i.request.path = self.text(&i.request.path);
            i.request.query = self.names(&i.request.query);
            for r in &mut i.responses {
                r.headers = self.names(&r.headers);
                // Per-request and trace ids identify one exchange: neither
                // kept nor refused.
                for (k, v) in &mut r.headers {
                    if trace_header(k) {
                        *v = REDACTED.to_owned();
                    }
                }
                r.json = r.json.as_ref().map(|j| self.json(j));
                r.body = r.body.as_deref().map(|b| self.text(b));
            }
        }
        out
    }

    /// Everything that still looks identifying.
    pub fn scan(&self, cassette: &Cassette) -> Vec<Finding> {
        let mut found = Vec::new();
        for (n, i) in cassette.interactions.iter().enumerate() {
            let at = format!("interactions[{n}]");
            self.scan_str(
                &format!("{at}.request.path"),
                None,
                &i.request.path,
                &mut found,
            );
            self.scan_names(&format!("{at}.request.query"), &i.request.query, &mut found);
            for (m, r) in i.responses.iter().enumerate() {
                let at = format!("{at}.responses[{m}]");
                self.scan_names(&format!("{at}.headers"), &r.headers, &mut found);
                if let Some(j) = &r.json {
                    self.scan_json(&format!("{at}.json"), None, j, &mut found);
                }
                if let Some(b) = &r.body {
                    self.scan_str(&format!("{at}.body"), None, b, &mut found);
                }
            }
        }
        found
    }

    fn report(&self, loc: &str, what: &'static str, found: &mut Vec<Finding>) {
        if !self.allowed.iter().any(|p| loc.starts_with(p.as_str())) {
            found.push(Finding {
                location: loc.to_owned(),
                what,
            });
        }
    }

    /// How a key or name at position `n` appears in locations: itself,
    /// unless it looks identifying.
    fn label(&self, name: &str, n: usize) -> String {
        if self.key_allowed(Some(name)) || detect(name, None).is_empty() {
            name.to_owned()
        } else {
            format!("<key {n}>")
        }
    }

    /// Findings in a key or name itself, at `<parent>.<label>#name`.
    fn scan_name(&self, member: &str, name: &str, found: &mut Vec<Finding>) {
        if self.key_allowed(Some(name)) {
            return;
        }
        let loc = format!("{member}#name");
        for what in detect(name, None) {
            self.report(&loc, what, found);
        }
    }

    /// Query parameters or headers: names, credentials, values. A trace
    /// header's value is not held to the id-key rule (redaction replaces
    /// it), but still to every other check.
    fn scan_names(&self, at: &str, map: &BTreeMap<String, String>, found: &mut Vec<Finding>) {
        for (n, (k, v)) in map.iter().enumerate() {
            let loc = format!("{at}.{}", self.label(k, n));
            self.scan_name(&loc, k, found);
            if secret_name(k) && v != REDACTED {
                self.report(&loc, "a credential", found);
            }
            if !self.key_allowed(Some(k)) {
                // As for JSON keys: any real value under an identifying name.
                if let Some(what) = identifying_key(k)
                    && !trace_header(k)
                    && !is_placeholder(&Value::String(v.clone()))
                {
                    self.report(&loc, what, found);
                }
                self.scan_str(&loc, Some(k), v, found);
            }
        }
    }

    /// `key` is the JSON key, query parameter or header the string is the
    /// value of, if any.
    fn scan_str(&self, loc: &str, key: Option<&str>, s: &str, found: &mut Vec<Finding>) {
        for what in detect(s, key) {
            self.report(loc, what, found);
        }
    }

    fn scan_json(&self, loc: &str, key: Option<&str>, v: &Value, found: &mut Vec<Finding>) {
        let scalar = !matches!(v, Value::Object(_) | Value::Array(_));
        if scalar && self.key_allowed(key) {
            return;
        }
        if let Some(k) = key {
            if let Some(what) = identifying_key(k)
                && scalar
                && !is_placeholder(v)
            {
                self.report(loc, what, found);
            }
            if precise_coordinate(k, v) {
                self.report(loc, "a precise coordinate (more than 2 decimals)", found);
            }
        }
        match v {
            Value::String(s) => self.scan_str(loc, key, s, found),
            Value::Number(n) => {
                let text = n.to_string();
                if long_digits(&text) && !timestamp(key, &text) {
                    self.report(loc, "an identifier (a long digit string)", found);
                }
            }
            Value::Array(a) => {
                for (n, x) in a.iter().enumerate() {
                    self.scan_json(&format!("{loc}[{n}]"), key, x, found);
                }
            }
            Value::Object(o) => {
                for (n, (k, x)) in o.iter().enumerate() {
                    let member = format!("{loc}.{}", self.label(k, n));
                    self.scan_name(&member, k, found);
                    self.scan_json(&member, Some(k), x, found);
                }
            }
            _ => {}
        }
    }
}

/// Whether `key` names a version (see the [module docs](self)):
/// `version`, `firmware` or `fw`, or one of a few named versions
/// (`firmware_version`, `fwVersion`, `sw-version`, …). A key that merely
/// ends in "version" (`serialVersion`) does not count.
fn version_like_key(key: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "firmware", "fw", "software", "sw", "hardware", "hw", "app", "os",
    ];
    let k = key.to_ascii_lowercase();
    if matches!(k.as_str(), "version" | "firmware" | "fw") {
        return true;
    }
    let Some(prefix) = k.strip_suffix("version") else {
        return false;
    };
    let prefix = prefix.strip_suffix(['_', '-']).unwrap_or(prefix);
    PREFIXES.contains(&prefix)
}

/// The most digits a version group may have, except one date-time group.
const MAX_VERSION_DIGITS: usize = 8;

/// The most digits a version may have in all, outside its one date-time
/// group: a serial split into short groups is not a version.
const MAX_VERSION_TOTAL: usize = 12;

/// A version group of 12 or 14 digits that reads as a date-time
/// (`YYYYMMDDhhmm[ss]`, years 2000 to 2049): a build stamp.
fn date_time_group(g: &str) -> bool {
    if !matches!(g.len(), 12 | 14) {
        return false;
    }
    let n = |r: std::ops::Range<usize>| g.get(r).and_then(|x| x.parse::<u32>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi)) =
        (n(0..4), n(4..6), n(6..8), n(8..10), n(10..12))
    else {
        return false;
    };
    let s = if g.len() == 14 { n(12..14) } else { Some(0) };
    (2000..2050).contains(&y)
        && (1..=12).contains(&mo)
        && (1..=31).contains(&d)
        && h < 24
        && mi < 60
        && s.is_some_and(|s| s < 60)
}

/// A dotted version: an optional `v`, digit groups joined by dots (at
/// least two, each of at most 8 digits, but for one 12- or 14-digit
/// date-time), and an optional `-` or `+` suffix of letters, digits and
/// dots; at most 12 digits in all outside the date-time group, the
/// suffix's included (however letters split them).
fn looks_like_version(s: &str) -> bool {
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let (core, suffix) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let groups: Vec<&str> = core.split('.').collect();
    let digits = |g: &&str| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit());
    let stamps = groups
        .iter()
        .filter(|g| g.len() > MAX_VERSION_DIGITS)
        .count();
    let stamp_digits: usize = groups
        .iter()
        .filter(|g| g.len() > MAX_VERSION_DIGITS)
        .map(|g| g.len())
        .sum();
    let core_digits: usize = groups.iter().map(|g| g.len()).sum::<usize>() - stamp_digits;
    let suffix_digits = suffix.map_or(0, |x| x.chars().filter(char::is_ascii_digit).count());
    groups.len() >= 2
        && core_digits + suffix_digits <= MAX_VERSION_TOTAL
        && groups.iter().all(digits)
        && groups
            .iter()
            .all(|g| g.len() <= MAX_VERSION_DIGITS || date_time_group(g))
        && stamps <= 1
        && suffix.is_none_or(|x| {
            !x.is_empty()
                && x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
                && !digit_run(x)
        })
}

/// A number (or numeric string) under a time-like key that reads as a
/// plausible epoch time.
fn timestamp(key: Option<&str>, text: &str) -> bool {
    key.is_some_and(time_like_key) && plausible_epoch(text)
}

/// What a string (the value of `key`, if any) looks like.
fn detect(s: &str, key: Option<&str>) -> Vec<&'static str> {
    let mut out = Vec::new();
    if looks_like_email(s) {
        out.push("an e-mail address");
    }
    if looks_like_mac(s) {
        out.push("a MAC address");
    }
    if looks_like_uuid(s) {
        out.push("a UUID");
    }
    if looks_like_ipv4(s) {
        out.push("an IPv4 address");
    }
    if looks_like_ipv6(s) {
        out.push("an IPv6 address");
    }
    if key.is_some_and(host_like_key) && looks_like_hostname(s) {
        out.push("a host name");
    }
    if looks_like_token(s) {
        out.push("a token");
    }
    // Under a version key a value is a version, not a decimal number:
    // unless it is version-shaped, ten digits in all are a finding,
    // however dots or letters split them.
    let long = if key.is_some_and(version_like_key) {
        !looks_like_version(s) && s.chars().filter(|c| c.is_numeric()).count() >= 10
    } else {
        // A string is not a number: a fraction can hide a serial.
        digit_run(s) && !timestamp(key, s)
    };
    if long {
        out.push("an identifier (a long digit string)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detectors() {
        // Positive vectors are assembled at run time, so the source text
        // itself never looks identifying to a leak scanner.
        let at = '@';
        assert!(looks_like_email(&format!("mail someone{at}home.net now")));
        assert!(!looks_like_email("someone@example.com"));
        assert!(looks_like_mac(
            &["aa", "bb", "cc", "dd", "ee", "01"].join(":")
        ));
        assert!(!looks_like_mac("00:00:00:00:00:00"));
        assert!(looks_like_ipv4(&format!("http://192.{}.1.20/status", 168)));
        assert!(!looks_like_ipv4("http://127.0.0.1:8080/"));
        assert!(!looks_like_ipv4("version 1.2.3.4.5"));
        assert!(looks_like_token("Bearer abcdefghijklmnopqrstuvwxyz"));
        assert!(!looks_like_token("Bearer REDACTED"));
        assert!(long_digits(&"1234567".repeat(2)));
        assert!(!long_digits("0000000000000"));
        assert!(precise_coordinate("latitude", &json!(12.3456)));
        assert!(!precise_coordinate("latitude", &json!(12.3)));
        assert!(is_placeholder(&json!("00A0000000")));
        assert!(is_placeholder(&json!("A-00000000")));
        assert!(!is_placeholder(&json!("SN-12345")));
    }

    #[test]
    fn secrets_in_headers_and_queries_are_replaced() {
        let mut i = Interaction::get("/v1/x")
            .query("api_key", "s3cret")
            .query("page", "2");
        i = i.json(200, json!({"ok": true}));
        i.responses[0]
            .headers
            .insert("Set-Cookie".into(), "sid=abc".into());
        let c = Cassette {
            interactions: vec![i],
        };
        let r = Redactor::new();
        assert_eq!(r.scan(&c).len(), 2);
        let clean = r.redact(&c);
        assert!(r.scan(&clean).is_empty());
        assert_eq!(clean.interactions[0].request.query["page"], "2");
    }

    // Identifying values are assembled at run time, so the source text
    // itself never looks identifying to a leak scanner.
    fn digits13() -> String {
        ["1234567", "890123"].concat()
    }

    fn uuid() -> String {
        ["0f1e2d3c", "4b5a", "6978", "8796", "a5b4c3d2e1f0"].join("-")
    }

    fn with_json(body: Value) -> Cassette {
        Cassette {
            interactions: vec![Interaction::get("/v1/x").json(200, body)],
        }
    }

    fn found(r: &Redactor, c: &Cassette) -> Vec<&'static str> {
        r.scan(c).into_iter().map(|f| f.what).collect()
    }

    #[test]
    fn digit_runs_are_found_inside_strings_and_numbers() {
        let r = Redactor::new();
        let path = Cassette {
            interactions: vec![
                Interaction::get(&format!("/v1/sites/{}/meters/x", digits13()))
                    .json(200, json!({})),
            ],
        };
        assert_eq!(found(&r, &path), ["an identifier (a long digit string)"]);
        let n: u64 = digits13().parse().unwrap();
        assert_eq!(
            found(&r, &with_json(json!({ "number": n }))),
            ["an identifier (a long digit string)"]
        );
        let text = Cassette {
            interactions: vec![
                Interaction::get("/v1/x").text(200, &format!("meter {} ok", digits13())),
            ],
        };
        assert!(!r.scan(&text).is_empty());
        // Fractional digits of a JSON number are not an identifier; in a
        // string they can hide one.
        assert!(
            r.scan(&with_json(json!({ "value": 19.123_456_789_012 })))
                .is_empty()
        );
        assert!(
            !r.scan(&with_json(json!({ "value": "19.1234567890123" })))
                .is_empty()
        );
    }

    #[test]
    fn uuids_macs_and_id_keys_are_found() {
        let r = Redactor::new();
        assert_eq!(
            found(&r, &with_json(json!({ "token_for": uuid() }))),
            ["a UUID"]
        );
        let bare = uuid().replace('-', "");
        assert_eq!(found(&r, &with_json(json!({ "x": bare }))), ["a UUID"]);
        let mac = ["0a1b", "2c3d", "4e5f"].join(".");
        assert_eq!(
            found(&r, &with_json(json!({ "x": mac }))),
            ["a MAC address"]
        );
        for key in [
            "id",
            "device_id",
            "deviceId",
            "zoneIDs",
            "serialNumber",
            "mac",
            "uuid",
        ] {
            assert!(
                !r.scan(&with_json(json!({ key: "abc" }))).is_empty(),
                "{key}"
            );
        }
        for key in ["valid", "humid", "paid", "identity_provider"] {
            assert!(
                r.scan(&with_json(json!({ key: "abc" }))).is_empty(),
                "{key}"
            );
        }
        // Placeholders pass.
        assert!(
            r.scan(&with_json(json!({ "device_id": "00A0000000", "id": 0 })))
                .is_empty()
        );
    }

    #[test]
    fn a_fraction_in_a_string_can_hide_a_serial() {
        let long = "an identifier (a long digit string)";
        for (key, v) in [
            (Some("meter"), "0.12345678901234"),
            (Some("note"), "1.12345678901"),
            (
                None,
                "\u{ff11}\u{ff12}\u{ff13}\u{ff14}\u{ff15}\u{ff16}\u{ff17}\u{ff18}\u{ff19}\u{ff10}\u{ff11}",
            ),
        ] {
            assert!(detect(v, key).contains(&long), "{key:?}: {v}");
        }
        // A JSON number's fraction is a measurement, not an identifier.
        let mut c = Cassette::default();
        c.push(
            Interaction::get("/v1/status")
                .json(200, serde_json::json!({ "meter": 19.123_456_789_012 })),
        );
        assert!(
            Redactor::new().scan(&c).is_empty(),
            "{:?}",
            Redactor::new().scan(&c)
        );
        let mut c = Cassette::default();
        c.push(
            Interaction::get("/v1/status")
                .json(200, serde_json::json!({ "meter": "19.123456789012" })),
        );
        assert_eq!(Redactor::new().scan(&c).len(), 1);
    }

    #[test]
    fn trace_headers_are_replaced_not_refused() {
        let names = [
            "x-request-id",
            "X-Request-Id",
            "x-amz-cf-id",
            "x-github-request-id",
            "x-trace-id",
            "CF-RAY",
        ];
        let mut c = Cassette::default();
        let mut r = Interaction::get("/v1/status").json(200, serde_json::json!({}));
        for n in names {
            r.responses[0]
                .headers
                .insert(n.to_owned(), "f3b2a1-AbCdEf_0".to_owned());
        }
        c.push(r);
        // Unredacted: values that are just opaque are not refused.
        assert!(
            Redactor::new().scan(&c).is_empty(),
            "{:?}",
            Redactor::new().scan(&c)
        );
        // Redacted: replaced with the placeholder.
        let out = Redactor::new().redact(&c);
        for n in names {
            assert_eq!(out.interactions[0].responses[0].headers[n], REDACTED, "{n}");
        }
        // Other checks still apply to their values: a long digit run, an
        // address.
        let mut bad = Cassette::default();
        let mut r = Interaction::get("/v1/status").json(200, serde_json::json!({}));
        r.responses[0]
            .headers
            .insert("x-request-id".into(), "req-12345678901".into());
        r.responses[0]
            .headers
            .insert("x-amz-cf-id".into(), "8.8.4.4".into());
        bad.push(r);
        let found = Redactor::new().scan(&bad);
        assert_eq!(found.len(), 2, "{found:?}");
        // Another id header is still held to the id-key rule.
        let mut other = Cassette::default();
        let mut r = Interaction::get("/v1/status").json(200, serde_json::json!({}));
        r.responses[0]
            .headers
            .insert("x-device-id".into(), "kitchen".into());
        other.push(r);
        assert_eq!(Redactor::new().scan(&other).len(), 1);
    }

    #[test]
    fn versions_pass_only_under_version_keys() {
        let long = "an identifier (a long digit string)";
        for (key, v) in [
            ("version", "1.4.20231215100512"),
            ("firmware_version", "v3.0.202312151005-rc.1"),
            ("fwVersion", "2.0.12345678+build.7"),
            ("firmware", "10.20231215100512"),
            ("sw-version", "1.2.3"),
        ] {
            assert!(version_like_key(key) && looks_like_version(v), "{key}: {v}");
            assert!(!detect(v, Some(key)).contains(&long), "{key}: {v}");
        }
        // Elsewhere, or not version-shaped, the digit-run rule stands.
        for (key, v) in [
            (Some("note"), "1.4.20231215100512"),
            (None, "1.4.20231215100512"),
            (Some("version"), "12345678901"),
            (Some("version"), "SN-12345678901"),
            (Some("version"), "1.2.12345678901-x/y"),
            (Some("versions"), "1.4.20231215100512"),
            // Groups of more than 8 digits that are not a date-time.
            (Some("version"), "0.123456789012345"),
            (Some("build"), "4012345678.1"),
            (Some("firmware"), "v12345678901.0"),
            (Some("version"), "1.20231315100512"),
            (Some("version"), "1.20231215100512.20231215100512"),
            // A serial split into short groups, or a date past the range.
            (Some("version"), "1.40123456.78901234"),
            (Some("appVersion"), "12345678.12345678"),
            (Some("fw"), "1.0-rc.123456789.123456789"),
            (Some("fw"), "1.0-a123456789b123456789"),
            (Some("firmware"), "1.20991231235959"),
            // A long digit run in the suffix.
            (Some("fw"), "1.0+00123456789012"),
            (Some("sw"), "1.2.3-SN12345678901"),
            (Some("fwVersion"), "1.2.3-SN12345678901"),
        ] {
            assert!(detect(v, key).contains(&long), "{key:?}: {v}");
        }
        // Not a version key: a serial key, so any value is a finding.
        let mut c = Cassette::default();
        c.push(
            Interaction::get("/v1/status")
                .json(200, serde_json::json!({ "serialVersion": "1.12345678901" })),
        );
        c.push(
            Interaction::get("/v1/info")
                .query("serialVersion", "1.12345678901")
                .json(200, serde_json::json!({})),
        );
        let found = Redactor::new().scan(&c);
        // A serial key, and a long digit run (in a string, the fraction
        // counts), in the body and in the query.
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(
            found.iter().all(|f| f.location.contains("serialVersion")),
            "{found:?}"
        );
        assert!(!version_like_key("build") && !version_like_key("sw"));
        assert!(!version_like_key("serialVersion") && !version_like_key("serial_version"));
        assert!(version_like_key("appVersion") && version_like_key("os_version"));
    }

    #[test]
    fn epoch_times_pass_only_under_time_like_keys() {
        let r = Redactor::new();
        let ms: u64 = 1_893_456_000_000;
        for key in [
            "timestamp",
            "time",
            "ts",
            "created_at",
            "updatedAt",
            "startTime",
        ] {
            assert!(r.scan(&with_json(json!({ key: ms }))).is_empty(), "{key}");
            assert!(
                r.scan(&with_json(json!({ key: ms.to_string() })))
                    .is_empty(),
                "{key}"
            );
        }
        assert!(
            r.scan(&with_json(json!({ "time": 1_893_456_000 })))
                .is_empty()
        );
        assert!(!r.scan(&with_json(json!({ "value": ms }))).is_empty());
        // Implausible as a time: still a finding.
        let n: u64 = digits13().parse::<u64>().unwrap() * 100;
        assert!(!r.scan(&with_json(json!({ "timestamp": n }))).is_empty());
        let q = Cassette {
            interactions: vec![
                Interaction::get("/v1/x")
                    .query("since", &ms.to_string())
                    .json(200, json!({})),
            ],
        };
        assert!(r.scan(&q).is_empty());
    }

    #[test]
    fn findings_can_be_allowed_explicitly() {
        let c = with_json(json!({ "id": "temperature", "items": [{ "id": "humidity" }] }));
        assert_eq!(Redactor::new().scan(&c).len(), 2);
        assert!(Redactor::new().allow_key("id").scan(&c).is_empty());
        let one = Redactor::new().allow("interactions[0].responses[0].json.id");
        assert_eq!(one.scan(&c).len(), 1);
    }

    #[test]
    fn every_ip_address_but_reserved_ones_is_found() {
        let v4 = |a: u8, b: u8, c: u8, d: u8| format!("{a}.{b}.{c}.{d}");
        for public in [v4(93, 184, 215, 14), v4(10, 1, 2, 3), v4(172, 20, 0, 9)] {
            assert!(
                looks_like_ipv4(&format!("http://{public}:80/x")),
                "{public}"
            );
        }
        for reserved in [
            v4(127, 0, 0, 1),
            v4(0, 0, 0, 0),
            v4(192, 0, 2, 7),
            v4(198, 51, 100, 7),
            v4(203, 0, 113, 7),
        ] {
            assert!(!looks_like_ipv4(&reserved), "{reserved}");
        }
        let v6 = ["2a00", "1450", "4009", "81f", "", "200e"].join(":");
        assert!(looks_like_ipv6(&format!("http://[{v6}]:80/")));
        assert!(looks_like_ipv6(&format!("from {v6}.")));
        for not in [
            "::1",
            "::",
            "2001:db8::42",
            "00:00:00",
            "12:30:45",
            "Type::method",
            "std::fs::read",
        ] {
            assert!(!looks_like_ipv6(not), "{not}");
        }
        assert!(!looks_like_ipv6(
            &["aa", "bb", "cc", "dd", "ee", "01"].join(":")
        ));
    }

    #[test]
    fn host_names_under_host_keys_are_found() {
        let host = ["hub", "home", "lan"].join(".");
        let r = Redactor::new();
        for key in [
            "host",
            "hostname",
            "mqtt_host",
            "brokerHost",
            "server",
            "fqdn",
        ] {
            assert_eq!(
                found(&r, &with_json(json!({ key: host }))),
                ["a host name"],
                "{key}"
            );
        }
        assert_eq!(
            found(
                &r,
                &with_json(json!({ "server": format!("https://{host}:8443/api") }))
            ),
            ["a host name"]
        );
        for fine in [
            "localhost",
            "api.localhost",
            "hub.example",
            "example.com",
            "api.example.org",
            "example.net",
            "hub.invalid",
            "hub.test",
            "REDACTED",
            "http://127.0.0.1:8080",
        ] {
            assert!(
                r.scan(&with_json(json!({ "host": fine }))).is_empty(),
                "{fine}"
            );
        }
        // Not a host-like key: a free-text value is not taken for a host.
        assert!(r.scan(&with_json(json!({ "label": host }))).is_empty());
    }

    #[test]
    fn names_are_scanned_and_never_printed() {
        let d = digits13();
        let key = with_json(json!({ "ok": 1, d.clone(): "x" }));
        let found = Redactor::new().scan(&key);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].location,
            "interactions[0].responses[0].json.<key 0>#name"
        );
        assert!(!found[0].to_string().contains(&d));
        let mut i = Interaction::get("/v1/x").query(&format!("n{d}"), "1");
        i = i.json(200, json!({}));
        i.responses[0].headers.insert(format!("x-{d}"), "1".into());
        let c = Cassette {
            interactions: vec![i],
        };
        let locations: Vec<String> = Redactor::new()
            .scan(&c)
            .into_iter()
            .map(|f| f.location)
            .collect();
        assert_eq!(
            locations,
            [
                "interactions[0].request.query.<key 0>#name",
                "interactions[0].responses[0].headers.<key 0>#name"
            ]
        );
        // Redaction reaches names too.
        let r = Redactor::new().literal(&d, "0000000000000");
        assert!(r.scan(&r.redact(&c)).is_empty());
        assert!(r.scan(&r.redact(&key)).is_empty());
        // So does allowing a name.
        assert!(Redactor::new().allow_key(&d).scan(&key).is_empty());
    }

    mod planted {
        use proptest::prelude::*;
        // Not wiremock's matcher of the same name.
        use proptest::prelude::any;

        use super::super::*;

        /// Where a planted value goes.
        #[derive(Debug, Clone, Copy)]
        enum Spot {
            Path,
            QueryValue,
            QueryName,
            HeaderValue,
            HeaderName,
            JsonString,
            NestedJsonString,
            JsonKey,
            NestedJsonKey,
            Body,
        }

        fn spot() -> impl Strategy<Value = Spot> {
            prop_oneof![
                Just(Spot::Path),
                Just(Spot::QueryValue),
                Just(Spot::QueryName),
                Just(Spot::HeaderValue),
                Just(Spot::HeaderName),
                Just(Spot::JsonString),
                Just(Spot::NestedJsonString),
                Just(Spot::JsonKey),
                Just(Spot::NestedJsonKey),
                Just(Spot::Body),
            ]
        }

        /// Text with no digits, dots or hex runs of its own.
        fn filler() -> impl Strategy<Value = String> {
            "[g-z/ _=-]{0,8}"
        }

        fn digits() -> impl Strategy<Value = String> {
            "[0-9]{10,24}".prop_filter("not all zeros", |s| s.bytes().any(|b| b != b'0'))
        }

        fn uuid() -> impl Strategy<Value = String> {
            (any::<[u8; 16]>(), any::<bool>(), any::<bool>())
                .prop_filter("not nil", |(b, ..)| b.iter().any(|&x| x != 0))
                .prop_map(|(b, upper, dashed)| {
                    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
                    let s = if dashed {
                        format!(
                            "{}-{}-{}-{}-{}",
                            &h[0..8],
                            &h[8..12],
                            &h[12..16],
                            &h[16..20],
                            &h[20..32]
                        )
                    } else {
                        h
                    };
                    if upper { s.to_ascii_uppercase() } else { s }
                })
                // A bare form needs letters and digits to stand out from a
                // plain number or word.
                .prop_filter("letters and digits", |s| {
                    s.contains('-')
                        || (s.bytes().any(|c| c.is_ascii_digit())
                            && s.bytes().any(|c| c.is_ascii_alphabetic()))
                })
        }

        fn mac() -> impl Strategy<Value = String> {
            (
                any::<[u8; 6]>(),
                prop_oneof![Just(':'), Just('-')],
                any::<bool>(),
            )
                .prop_filter("not all zeros", |(b, ..)| b.iter().any(|&x| x != 0))
                .prop_map(|(b, sep, upper)| {
                    let s = b
                        .iter()
                        .map(|x| format!("{x:02x}"))
                        .collect::<Vec<_>>()
                        .join(&sep.to_string());
                    if upper { s.to_ascii_uppercase() } else { s }
                })
        }

        fn ipv4() -> impl Strategy<Value = String> {
            any::<[u8; 4]>()
                .prop_filter("not reserved", |o| {
                    !(o[0] == 127
                        || *o == [0, 0, 0, 0]
                        || matches!(o, [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]))
                })
                .prop_map(|o| format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]))
        }

        fn identifying() -> impl Strategy<Value = String> {
            prop_oneof![digits(), uuid(), mac(), ipv4()]
        }

        /// A cassette with `text` planted at `spot`, and the location a
        /// finding must name.
        fn plant(spot: Spot, text: &str) -> (Cassette, String) {
            let mut i = Interaction::get("/v1/status");
            let mut body = json!({ "temperature": 20.5, "label": "ok" });
            let response = "interactions[0].responses[0]";
            let key_at = |obj: &Value| {
                obj.as_object()
                    .and_then(|o| o.keys().position(|k| k == text))
                    .unwrap_or(usize::MAX)
            };
            let loc = match spot {
                Spot::Path => {
                    i.request.path = format!("/v1/{text}");
                    "interactions[0].request.path".to_owned()
                }
                Spot::QueryValue => {
                    i.request.query.insert("page".into(), text.to_owned());
                    "interactions[0].request.query.page".to_owned()
                }
                Spot::QueryName => {
                    i.request.query.insert(text.to_owned(), "1".into());
                    "interactions[0].request.query.<key 0>#name".to_owned()
                }
                Spot::JsonString => {
                    body["label"] = json!(text);
                    format!("{response}.json.label")
                }
                Spot::NestedJsonString => {
                    body["items"] = json!([{ "note": ["ok", text] }]);
                    format!("{response}.json.items[0].note[1]")
                }
                Spot::JsonKey => {
                    body[text] = json!("ok");
                    format!("{response}.json.<key {}>#name", key_at(&body))
                }
                Spot::NestedJsonKey => {
                    body["items"] = json!([{ text: "ok" }]);
                    format!("{response}.json.items[0].<key 0>#name")
                }
                Spot::HeaderValue => format!("{response}.headers.x-note"),
                Spot::HeaderName => format!("{response}.headers.<key 0>#name"),
                Spot::Body => format!("{response}.body"),
            };
            i = if let Spot::Body = spot {
                i.text(200, text)
            } else {
                i.json(200, body)
            };
            match spot {
                Spot::HeaderValue => {
                    i.responses[0]
                        .headers
                        .insert("x-note".into(), text.to_owned());
                }
                Spot::HeaderName => {
                    i.responses[0].headers.insert(text.to_owned(), "1".into());
                }
                _ => {}
            }
            let c = Cassette {
                interactions: vec![i],
            };
            (c, loc)
        }

        fn refused_at(c: &Cassette, loc: &str) -> Result<(), TestCaseError> {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "heimr-testkit-planted-{}-{n}.json",
                std::process::id()
            ));
            let result = c.save(&path, &Redactor::new());
            let written = path.exists();
            let _ = std::fs::remove_file(&path);
            prop_assert!(!written);
            match result {
                Err(CassetteError::Identifying(f)) => prop_assert!(
                    f.iter().any(|x| x.location == loc),
                    "no finding at {loc}: {f:?}"
                ),
                other => prop_assert!(false, "not refused: {other:?} for {c:?}"),
            }
            Ok(())
        }

        proptest! {
            #[test]
            fn the_unplanted_cassette_is_clean(spot in spot(), pad in filler()) {
                let (c, _) = plant(spot, &format!("x{pad}"));
                prop_assert!(Redactor::new().scan(&c).is_empty(), "{:?}", Redactor::new().scan(&c));
            }

            #[test]
            fn a_planted_value_is_refused_at_its_location(
                spot in spot(),
                before in filler(),
                value in identifying(),
                after in filler(),
            ) {
                let (c, loc) = plant(spot, &format!("{before}{value}{after}"));
                refused_at(&c, &loc)?;
            }

            #[test]
            fn a_planted_number_is_refused_at_its_location(
                n in prop_oneof![
                    (1_000_000_000u64..=u64::MAX).prop_map(Value::from),
                    (i64::MIN..=-1_000_000_000i64).prop_map(Value::from),
                ],
                nested in any::<bool>(),
            ) {
                let (body, loc) = if nested {
                    (json!({ "items": [{ "reading": n }] }), "items[0].reading")
                } else {
                    (json!({ "reading": n }), "reading")
                };
                let c = Cassette { interactions: vec![Interaction::get("/v1/x").json(200, body)] };
                refused_at(&c, &format!("interactions[0].responses[0].json.{loc}"))?;
            }
        }
    }
}
