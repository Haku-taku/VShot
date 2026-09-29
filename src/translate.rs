// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 VShot contributors

//! Screenshot translation, for `vshot translate` and the editor's translate
//! overlay.
//!
//! Nine services can do the work, and the config file picks which.  The
//! selection may also be `auto`, which tries the usable providers in a
//! built-in order, and `cli.translate.fallback` names more providers to try
//! when the chosen one produces nothing:
//!
//! - Five of them need no account and no key **of your own**, which is what
//!   makes them the ones `auto` reaches for first.  `google` (the default) is
//!   the endpoint the official Android app calls, and `microsoft`,
//!   `volcengine`, `transmart` and `lingocloud` are the tokenless backends
//!   other clients use.  `google` and `volcengine` answer a single blob at a
//!   time and say nothing about which input line a piece of it began in, so
//!   both are asked once per line and the lines are put back in order here;
//!   `microsoft`, `transmart` and `lingocloud` take several lines in one
//!   request.  `lingocloud` is the fifth and the only one running on a
//!   credential that is not vshot's — a borrowed token (see the note on
//!   `LINGOCLOUD_TOKEN`) — so `auto` reaches it only after the other four
//!   have failed.
//! - `bing` is Azure's Translator, `baidu` is Baidu's fanyi, and `ai` is any
//!   OpenAI-compatible chat-completions endpoint.  Each needs an account and
//!   a key (see the README); all three accept several lines in one request.
//! - `external` runs a command of the user's choosing, exactly as the OCR
//!   engine's `external` does: the source lines go in on stdin and the
//!   translations come out on stdout.  It is the escape hatch for a service
//!   the other eight do not cover, and the only one that works with nothing
//!   but a shell script.
//!
//! All of them answer one question — a batch of source lines in, the same
//! number of translated lines out, in the same order — so the rest of the
//! program never has to know which one ran.
//!
//! A chain of them is tried when the primary fails to produce anything.  A
//! provider counts as having worked when **at least one line** translated:
//! Google answers a throttled client with a page it cannot parse, or with
//! per-line errors under a 200, so a chain that only reacted to a whole-batch
//! failure would never recover from the case it exists for (see
//! [`translate_chain`] and [`engine_chain`]).
//!
//! The result is the OCR envelope with its `lines[].text` replaced and the
//! original kept as `source` (see [`translate_envelope_chain`]).  That is deliberate:
//! the Qt side feeds it straight into the text layer it already parses for
//! `vshot ocr --json`, and the translated text lands at the original
//! positions for free.  `chars` is **dropped**: per-character boxes exist so a
//! reader can select the recognized text character by character, and the
//! translated overlay does not select the source at all.  `geometry` stays
//! `true` all the same, because the line rects are real and are what the Qt
//! text layer places the translation with — the flag describes the rects, not
//! the boxes that were dropped.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use md5::{Digest, Md5};
use serde_json::{json, Value};

use crate::config::TranslateDefaults;
use crate::error::{Result, VshotError};

/// The Google endpoint, spelled out rather than built from parts: it is the
/// one URL here with no knob a user would ever change.
const GOOGLE_ENDPOINT: &str = "https://translate.google.com/translate_a/single";

/// The user agent the official Google Translate Android app sends.  It is the
/// whole reason this endpoint answers with JSON: from a generic user agent the
/// same URL comes back as a captcha page instead.  Do not "clean this up" — the
/// spoof is the recipe, and a future reader who drops it gets a wall of HTML.
const GOOGLE_ANDROID_USER_AGENT: &str =
    "GoogleTranslate/6.14.0.04.343003216 (Linux; U; Android 10; Redmi K20 Pro)";

/// Microsoft's tokenless edge endpoint: no key, no token, no handshake.  This
/// is not the `/translate/auth` endpoint (that one is a 404); it needs none.
const MICROSOFT_ENDPOINT: &str = "https://edge.microsoft.com/translate/translatetext";

/// Volcengine's endpoint, the one the vendor's own Chrome extension calls.
const VOLCENGINE_ENDPOINT: &str = "https://translate.volcengine.com/crx/translate/v1/";

/// The origin Volcengine's endpoint checks: the id of the vendor's own Chrome
/// extension, so a request that does not claim to come from it is refused.
/// Like the Google user agent above, this header is load-bearing.
const VOLCENGINE_ORIGIN: &str = "chrome-extension://klgfhbiooeogfpknjdcbablpceialkdj";

/// Tencent's Transmart endpoint.
const TRANSMART_ENDPOINT: &str = "https://transmart.qq.com/api/imt";

/// A current desktop Chrome user agent.  Transmart and Volcengine both expect
/// to be called by a browser and check for one.
const CHROME_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Caiyun's interpreter endpoint, the one the vendor's web app calls.  It is a
/// batch API: one translated string comes back per input line, in order.
const LINGOCLOUD_ENDPOINT: &str = "https://api.interpreter.caiyunai.com/v1/translator";

/// The user agent Caiyun's client sends.  It is not a browser's — the
/// endpoint takes it as written — so it is spelled out rather than falling
/// back to the shared agent's `vshot/...`.
const LINGOCLOUD_USER_AGENT: &str = "okhttp/3.12.3";

/// The token Caiyun's own web app uses, lifted from it into a client and then
/// passed between the third-party Telegram forks ever since.  It is **not
/// vshot's and not Nagram's either**: the same constant appears in dozens of
/// forks (NekoX, Nekogram, Nullgram, OctoGram, AdvanceGram and their
/// descendants), where it dates back to a "Merge translate providers" commit
/// around 2020-12/2021-01.  Caiyun can revoke it at any time; when it does,
/// the fix is a token of your own under `cli.translate.lingocloud.token` (see
/// the README), not a code change here — which is why the config overrides
/// this constant.
const LINGOCLOUD_TOKEN: &str = "9sdftiq37bnv410eon2l";

/// Azure's default regional endpoint; `translate.bing.endpoint` overrides it
/// for a single-region resource.
const BING_ENDPOINT: &str = "https://api.cognitive.microsofttranslator.com";

/// Baidu's API has exactly one address; only the credentials vary.
const BAIDU_ENDPOINT: &str = "https://fanyi-api.baidu.com/api/trans/vip/translate";

/// A named client: Bing rejects a request with no user agent, and a server's
/// logs can tell vshot's traffic apart from a browser's.
const USER_AGENT: &str = concat!("vshot/", env!("CARGO_PKG_VERSION"));

/// How many Google requests may be in flight at once.  The endpoint takes one
/// line per request and the replies come back out of order; four is enough to
/// hide the round-trip without looking like an attack on a free service.
const GOOGLE_IN_FLIGHT: usize = 4;

/// Which service does the work, without its settings.  This is the part the
/// config file names and the part the language tables hang off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Google,
    Microsoft,
    Volcengine,
    Transmart,
    Lingocloud,
    Bing,
    Baidu,
    Ai,
    External,
}

impl Kind {
    /// Reads the provider name from the config.  An unknown name is an error
    /// rather than a fallback: a user who wrote `deepL` wants to hear that it
    /// is not a provider, not silently get Google output.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "google" => Ok(Self::Google),
            "microsoft" => Ok(Self::Microsoft),
            "volcengine" => Ok(Self::Volcengine),
            "transmart" => Ok(Self::Transmart),
            "lingocloud" => Ok(Self::Lingocloud),
            "bing" => Ok(Self::Bing),
            "baidu" => Ok(Self::Baidu),
            "ai" => Ok(Self::Ai),
            "external" => Ok(Self::External),
            other => Err(VshotError::Translate(format!(
                "unknown translate provider `{other}`: expected google, microsoft, volcengine, \
                 transmart, lingocloud, bing, baidu, ai or external"
            ))),
        }
    }

    /// The name the config file and `--provider` use.
    pub fn word(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Microsoft => "microsoft",
            Self::Volcengine => "volcengine",
            Self::Transmart => "transmart",
            Self::Lingocloud => "lingocloud",
            Self::Bing => "bing",
            Self::Baidu => "baidu",
            Self::Ai => "ai",
            Self::External => "external",
        }
    }

    /// How this provider spells `tag` as a *source* language.  `None` means the
    /// request has to leave the source out: that is Bing's and Microsoft's own
    /// way of saying "detect it", and Volcengine never takes a source at all.
    /// The rest spell detection with a word.
    ///
    /// A tag the table does not know passes through unchanged: a provider that
    /// does understand it is better served by the word, and one that does not
    /// will say so, which is more honest than silently dropping the language.
    pub fn source_tag(self, tag: &str) -> Option<String> {
        match self {
            Self::Bing => (tag != "auto").then(|| tag.to_owned()),
            Self::Microsoft => (tag != "auto").then(|| microsoft_tag(tag)),
            // Volcengine's request carries no source field at all: detection is
            // implicit, so there is nothing a tag could fill.
            Self::Volcengine => None,
            Self::Transmart => Some(transmart_tag(tag)),
            Self::Lingocloud => Some(lingocloud_tag(tag)),
            Self::Google => Some(google_tag(tag)),
            Self::Baidu => Some(baidu_tag(tag)),
            Self::Ai => Some(tag.to_owned()),
            Self::External => Some(tag.to_owned()),
        }
    }

    /// How this provider spells `tag` as a *target* language.
    pub fn target_tag(self, tag: &str) -> String {
        match self {
            Self::Google => google_tag(tag),
            Self::Microsoft => microsoft_tag(tag),
            Self::Volcengine => volcengine_tag(tag),
            Self::Transmart => transmart_tag(tag),
            Self::Lingocloud => lingocloud_tag(tag),
            Self::Bing => tag.to_owned(),
            Self::Baidu => baidu_tag(tag),
            Self::Ai => tag.to_owned(),
            Self::External => tag.to_owned(),
        }
    }
}

/// The `--provider`/`cli.translate.provider` value meaning "pick the first
/// usable provider": a selector, not a provider, so it is not a [`Kind`].
pub const AUTO: &str = "auto";

/// The order `auto` tries the providers in, and the rule for what "usable"
/// means.  The five providers that need no account of your own come first, in
/// the order the user is most likely to get an answer from, and are always
/// usable; `bing`, `baidu` and `ai` need their credentials, and `external`
/// needs a command, so each of those is skipped until its settings are present.
///
/// `lingocloud` is the last of the credential-free five, right before the
/// keyed ones: it needs no config, but it runs on a token that belongs to
/// somebody else, so `auto` should exhaust the four truly keyless providers
/// before borrowing it (see `LINGOCLOUD_TOKEN`).
pub const AUTO_ORDER: [Kind; 9] = [
    Kind::Google,
    Kind::Microsoft,
    Kind::Volcengine,
    Kind::Transmart,
    Kind::Lingocloud,
    Kind::Bing,
    Kind::Baidu,
    Kind::Ai,
    Kind::External,
];

/// Checks a `--provider` or `cli.translate.provider` value: one of the nine
/// provider names, or `auto`.  This is the one place the extra spelling is
/// accepted, so `Kind::parse` stays a provider parser.
pub fn validate_provider(name: &str) -> Result<()> {
    if name == AUTO {
        return Ok(());
    }
    Kind::parse(name).map(|_| ())
}

/// Google's own spelling: it calls simplified and traditional Chinese `zh-CN`
/// and `zh-TW`, and everything else the way vshot already writes it.
fn google_tag(tag: &str) -> String {
    match tag {
        "zh-Hans" => "zh-CN".to_owned(),
        "zh-Hant" => "zh-TW".to_owned(),
        other => other.to_owned(),
    }
}

/// Microsoft's spelling: it takes vshot's tags as they are, `zh-Hans` and
/// `zh-Hant` included.  The function is here for symmetry with the other
/// providers, so a future mapping has an obvious home.
fn microsoft_tag(tag: &str) -> String {
    tag.to_owned()
}

/// Volcengine's spelling.  It is [`microsoft_tag`] except for one code:
/// simplified Chinese is `zh`, not `zh-Hans`.  The live endpoint does not know
/// `zh-Hans` (nor `zh-CN`/`zh-TW`) and quietly answers in English when asked
/// for one, so `zh` is the code that actually translates; `zh-Hant` is taken
/// as written and does yield traditional Chinese.
fn volcengine_tag(tag: &str) -> String {
    match tag {
        "zh-Hans" => "zh".to_owned(),
        other => other.to_owned(),
    }
}

/// Tencent Transmart's spelling: both Chinese scripts collapse to `zh`, and
/// everything else — `auto` included — is taken as written.
fn transmart_tag(tag: &str) -> String {
    match tag {
        "zh-Hans" | "zh-Hant" => "zh".to_owned(),
        other => other.to_owned(),
    }
}

/// Caiyun's spelling, for the `<from>2<to>` pair it splices together.
///
/// Only `zh-Hans` is rewritten, to `zh`: the endpoint rejects it outright
/// (`rc=-1 Unsupported trans_type`) on either side of the pair — as a target
/// (`ja2zh-Hans`) and as a source (`zh-Hans2ja`) alike — while plain `zh` is
/// its code for simplified Chinese.  Without the rewrite vshot's own default
/// target would be a hard error.
///
/// `zh-Hant` is **not** rewritten.  It is accepted as written: as a target it
/// yields traditional Chinese (a live `ja2zh-Hant` / `auto2zh-Hant` comes back
/// with 繁體 — 今天天氣真好啊。 — not the simplified 今天天气真好啊。), and as a
/// source the endpoint silently normalises it to `zh` (`zh-Hant2ja` echoes
/// `zh2ja`).  Collapsing it to `zh` would throw away a working traditional
/// path and make the provider's output worse, so it passes through.
///
/// Everything else — `auto` included — is taken as written, the same rule the
/// rest of the module follows: the endpoint translates more languages (`de`,
/// `ko`, ...) than the six a web-app client would offer, and a tag it does not
/// know is left to the service's own `rc=-1` rather than dropped here.
fn lingocloud_tag(tag: &str) -> String {
    match tag {
        "zh-Hans" => "zh".to_owned(),
        other => other.to_owned(),
    }
}

/// Baidu's spelling, which is its own: `zh`/`cht` for the two Chinese scripts,
/// `jp`/`kor`/`fra` for three languages whose BCP-47 codes it does not take.
fn baidu_tag(tag: &str) -> String {
    match tag {
        "zh-Hans" => "zh".to_owned(),
        "zh-Hant" => "cht".to_owned(),
        "ja" => "jp".to_owned(),
        "ko" => "kor".to_owned(),
        "fr" => "fra".to_owned(),
        other => other.to_owned(),
    }
}

/// One translated line, or the reason that line could not be translated.  A
/// per-line failure is kept next to its source rather than failing the batch:
/// one bad line must never lose the rest of the translation.
pub type LineResult = std::result::Result<String, String>;

/// The provider boundary: a batch of source lines in, one result per line out,
/// in order.  It exists as a trait so a test can stand in for the network.
pub trait Translator {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>>;
}

/// A provider, resolved from the config file with everything it needs to run.
#[derive(Clone, Debug)]
pub enum Engine {
    Google(GoogleEngine),
    Microsoft(MicrosoftEngine),
    Volcengine(VolcengineEngine),
    Transmart(TransmartEngine),
    Lingocloud(LingocloudEngine),
    Bing(BingEngine),
    Baidu(BaiduEngine),
    Ai(AiEngine),
    External(ExternalEngine),
}

impl Engine {
    /// The provider name the envelope reports and the error messages name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Google(_) => Kind::Google.word(),
            Self::Microsoft(_) => Kind::Microsoft.word(),
            Self::Volcengine(_) => Kind::Volcengine.word(),
            Self::Transmart(_) => Kind::Transmart.word(),
            Self::Lingocloud(_) => Kind::Lingocloud.word(),
            Self::Bing(_) => Kind::Bing.word(),
            Self::Baidu(_) => Kind::Baidu.word(),
            Self::Ai(_) => Kind::Ai.word(),
            Self::External(_) => Kind::External.word(),
        }
    }
}

impl Translator for Engine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        match self {
            Self::Google(engine) => engine.translate(from, to, lines),
            Self::Microsoft(engine) => engine.translate(from, to, lines),
            Self::Volcengine(engine) => engine.translate(from, to, lines),
            Self::Transmart(engine) => engine.translate(from, to, lines),
            Self::Lingocloud(engine) => engine.translate(from, to, lines),
            Self::Bing(engine) => engine.translate(from, to, lines),
            Self::Baidu(engine) => engine.translate(from, to, lines),
            Self::Ai(engine) => engine.translate(from, to, lines),
            Self::External(engine) => engine.translate(from, to, lines),
        }
    }
}

/// Resolves the provider from the config file.  A provider that is named but
/// unusable — Bing without a key, `ai` without a model — is an error rather
/// than a fallback: the user asked for that service, and quietly translating
/// with another would be worse than not translating at all.
///
/// `provider` is the already-resolved name (the `--provider` flag, else
/// `cli.translate.provider`), passed in rather than read here so one place
/// decides the priority order.
pub fn engine_from_config(defaults: &TranslateDefaults, provider: &str) -> Result<Engine> {
    let kind = Kind::parse(provider)?;
    let timeout = Duration::from_secs(defaults.timeout.unwrap_or(20).max(1));
    match kind {
        Kind::Google => Ok(Engine::Google(GoogleEngine {
            agent: agent(timeout),
        })),
        // The keyless providers that take no settings have nothing to read out
        // of the config: a bare config resolves them.
        Kind::Microsoft => Ok(Engine::Microsoft(MicrosoftEngine {
            agent: agent(timeout),
        })),
        Kind::Volcengine => Ok(Engine::Volcengine(VolcengineEngine {
            agent: agent(timeout),
        })),
        Kind::Transmart => Ok(Engine::Transmart(TransmartEngine {
            agent: agent(timeout),
        })),
        Kind::Lingocloud => {
            // The one credential-free-by-config provider with an optional
            // override: the built-in token is borrowed from Caiyun's web app
            // (see `LINGOCLOUD_TOKEN`), and a user with their own Caiyun
            // account stops borrowing by setting `cli.translate.lingocloud.token`.
            // An absent or blank key leaves the built-in in place.
            let lingocloud = defaults.lingocloud.clone().unwrap_or_default();
            let token = non_empty(lingocloud.token).unwrap_or_else(|| LINGOCLOUD_TOKEN.to_owned());
            Ok(Engine::Lingocloud(LingocloudEngine {
                agent: agent(timeout),
                token,
            }))
        }
        Kind::Bing => {
            let bing = defaults.bing.clone().unwrap_or_default();
            let api_key = bing.api_key.unwrap_or_default();
            if api_key.trim().is_empty() {
                return Err(VshotError::Translate(
                    "bing needs an api-key: set cli.translate.bing.api-key".into(),
                ));
            }
            let endpoint = non_empty(bing.endpoint).unwrap_or_else(|| BING_ENDPOINT.to_owned());
            Ok(Engine::Bing(BingEngine {
                agent: agent(timeout),
                endpoint,
                api_key,
                region: non_empty(bing.region),
            }))
        }
        Kind::Baidu => {
            let baidu = defaults.baidu.clone().unwrap_or_default();
            let app_id = baidu.app_id.unwrap_or_default();
            let secret_key = baidu.secret_key.unwrap_or_default();
            if app_id.trim().is_empty() || secret_key.trim().is_empty() {
                return Err(VshotError::Translate(
                    "baidu needs an app-id and a secret-key: set cli.translate.baidu.app-id and \
                     cli.translate.baidu.secret-key"
                        .into(),
                ));
            }
            Ok(Engine::Baidu(BaiduEngine {
                agent: agent(timeout),
                app_id,
                secret_key,
            }))
        }
        Kind::Ai => {
            let ai = defaults.ai.clone().unwrap_or_default();
            let endpoint = ai.endpoint.unwrap_or_default();
            let api_key = ai.api_key.unwrap_or_default();
            let model = ai.model.unwrap_or_default();
            if endpoint.trim().is_empty() {
                return Err(VshotError::Translate(
                    "ai needs an endpoint: set cli.translate.ai.endpoint".into(),
                ));
            }
            if api_key.trim().is_empty() {
                return Err(VshotError::Translate(
                    "ai needs an api-key: set cli.translate.ai.api-key".into(),
                ));
            }
            if model.trim().is_empty() {
                return Err(VshotError::Translate(
                    "ai needs a model: set cli.translate.ai.model".into(),
                ));
            }
            Ok(Engine::Ai(AiEngine {
                agent: agent(timeout),
                endpoint,
                api_key,
                model,
                prompt: ai.prompt.unwrap_or_default(),
            }))
        }
        Kind::External => {
            let external = defaults.external.clone().unwrap_or_default();
            let command = external.command.unwrap_or_default();
            if command.is_empty() {
                return Err(VshotError::Translate(
                    "translate.provider is \"external\" but no cli.translate.external.command is \
                     configured"
                        .into(),
                ));
            }
            Ok(Engine::External(ExternalEngine {
                command,
                timeout: Duration::from_secs(external.timeout.unwrap_or(30).max(1)),
            }))
        }
    }
}

/// Resolves the usable providers from `order`, in order.
///
/// A provider is usable when [`engine_from_config`] can build it: the
/// credentialled services and `external` are dropped until their settings are
/// present, and each one's reason (the same "needs an api-key" message the
/// explicit route gives) is collected.  An order with nothing usable is an
/// error naming every missing piece; `AUTO_ORDER` always contains `google`, so
/// that error is only reachable through this function directly.
fn pick_usable(defaults: &TranslateDefaults, order: &[Kind]) -> Result<Vec<(String, Engine)>> {
    let mut engines = Vec::new();
    let mut missing = Vec::new();
    for kind in order {
        match engine_from_config(defaults, kind.word()) {
            Ok(engine) => engines.push((kind.word().to_owned(), engine)),
            Err(error) => missing.push(error.to_string()),
        }
    }
    if engines.is_empty() {
        return Err(VshotError::Translate(format!(
            "no translate provider is usable: {}",
            missing.join("; ")
        )));
    }
    Ok(engines)
}

/// Resolves the ordered providers to try: the primary, or the whole usable
/// `AUTO_ORDER` when the primary is `auto`, followed by the `fallback` names.
///
/// The chain is what makes a throttled provider recoverable.  A name already
/// in the chain is skipped, so listing `google` again under a `google` primary
/// — or under `auto`, which already contains it — does not run it twice.  An
/// unknown name is refused by [`Kind::parse`], the same way the primary is.
/// A named provider without its credentials is an error, not a silent skip:
/// the user asked for it, so being told what it is missing beats a chain that
/// quietly is not what they wrote.
pub fn engine_chain(defaults: &TranslateDefaults, provider: &str) -> Result<Vec<(String, Engine)>> {
    let mut engines = if provider == AUTO {
        pick_usable(defaults, &AUTO_ORDER)?
    } else {
        vec![(provider.to_owned(), engine_from_config(defaults, provider)?)]
    };
    for entry in defaults.fallback.iter().flatten() {
        Kind::parse(entry)?;
        if engines.iter().any(|(name, _)| name == entry) {
            continue;
        }
        engines.push((entry.clone(), engine_from_config(defaults, entry)?));
    }
    Ok(engines)
}

/// Borrows a resolved chain as the trait objects [`translate_envelope_chain`]
/// takes, so a caller does not have to spell out the cast.  The name comes
/// from the engine itself, which is also what the envelope reports.
pub fn as_providers(chain: &[(String, Engine)]) -> Vec<(&str, &dyn Translator)> {
    chain
        .iter()
        .map(|(_, engine)| (engine.name(), engine as &dyn Translator))
        .collect()
}

/// A trimmed value, or `None` when the config left it blank.  An empty string
/// in a hand-edited file means "not set" everywhere the config is read.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// The HTTP agent every provider shares, built once with the configured
/// timeout.  `http_status_as_error(false)` keeps a non-2xx response readable
/// so its body can be quoted in the error, which is where an API's own
/// explanation lives.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .build()
        .into()
}

/// One HTTP request a provider is about to send, kept apart from sending it so
/// the tests can pin the exact URL, headers and body — a refactor that breaks
/// Baidu's signature or Bing's `from` parameter fails there instead of on a
/// user's screen.
#[derive(Debug, PartialEq)]
struct HttpRequest {
    method: &'static str,
    url: String,
    headers: Vec<(&'static str, String)>,
    body: Option<String>,
}

/// The "try again" statuses: the service is asking for a moment's patience,
/// not reporting that the request was wrong.  Every other status is the answer
/// — a bad key or a malformed request never succeeds on a second try — so only
/// these are retried.
fn retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// How long to wait between attempts.  Short on purpose: the editor calls this
/// route while its translate window is up, so a retry must never make the UI
/// feel stuck.  The pauses are bounded by the configured timeout as well — see
/// [`with_retries`].
const RETRY_BACKOFFS: [Duration; 2] = [Duration::from_millis(300), Duration::from_millis(900)];

/// One attempt's outcome, with the failure already sorted into the two kinds
/// the retry loop treats differently.
enum Attempt<T> {
    /// The request succeeded.
    Done(T),
    /// A failure a retry may fix: a transport error, or a "try again" status.
    Transient(VshotError),
    /// A failure a retry cannot fix: anything else that came back.
    Fatal(VshotError),
}

/// Runs up to `backoffs.len() + 1` attempts of `attempt`, pausing `backoffs`
/// between them, and never past `timeout` from the first attempt.
///
/// A `Transient` failure is retried; a `Fatal` one stops at once.  The final
/// error is the last attempt's, with the number of attempts appended, so a
/// caller can tell a one-off flake from an outage.  The whole loop — sleeps
/// included — is bounded by `timeout`, so a dead network cannot make it run
/// three times as long as the caller asked.  `attempt` is a closure so the
/// loop can be exercised without a network.
fn with_retries<T>(
    timeout: Duration,
    backoffs: &[Duration],
    mut attempt: impl FnMut() -> Attempt<T>,
) -> Result<T> {
    let deadline = std::time::Instant::now() + timeout;
    let attempts = backoffs.len() + 1;
    let mut made = 0;
    let mut last = None;
    for index in 0..attempts {
        made += 1;
        match attempt() {
            Attempt::Done(value) => return Ok(value),
            Attempt::Fatal(error) => return Err(error),
            Attempt::Transient(error) => last = Some(error),
        }
        // The last allowed attempt has no pause after it.
        if index == backoffs.len() {
            break;
        }
        let pause = backoffs[index];
        // A pause that would run past the deadline is not worth starting.
        if std::time::Instant::now() + pause >= deadline {
            break;
        }
        std::thread::sleep(pause);
    }
    let error = last.expect("a transient failure leaves an error behind");
    let noun = if made == 1 { "attempt" } else { "attempts" };
    // The message is carried, not the error that was handed in: a `Translate`'s
    // own `Display` already reads "translation failed: ...", and nesting one
    // inside another is what made a throttled Google print the phrase twice.
    let detail = match error {
        VshotError::Translate(message) => message,
        other => other.to_string(),
    };
    Err(VshotError::Translate(format!(
        "{detail} (gave up after {made} {noun})"
    )))
}

/// Sends a request and returns its body, or a provider error that names the
/// provider and quotes whatever the service said.
///
/// A transport failure or a "try again" status is retried a few times (see
/// [`with_retries`]); the timeout is read back from the agent, so the retries
/// share the one the provider was built with.
fn send(agent: &ureq::Agent, request: &HttpRequest, provider: &str) -> Result<String> {
    send_parsed(agent, request, provider, |body| {
        Attempt::Done(body.to_owned())
    })
}

/// Like [`send`], but the caller sorts a would-be success's body into `Done`,
/// `Transient` or `Fatal` before it is taken as the answer.
///
/// A provider whose 2xx body can itself be a throttling page (Google) needs
/// the retry loop to see that shape; handing the body to `classify` here is
/// how the body check joins the status check in the one retry policy instead
/// of a retry of its own.
fn send_parsed<T>(
    agent: &ureq::Agent,
    request: &HttpRequest,
    provider: &str,
    classify: impl Fn(&str) -> Attempt<T>,
) -> Result<T> {
    let timeout = agent
        .config()
        .timeouts()
        .global
        .unwrap_or(Duration::from_secs(20));
    with_retries(timeout, &RETRY_BACKOFFS, || {
        match one_attempt(agent, request, provider) {
            Attempt::Done(body) => classify(&body),
            Attempt::Transient(error) => Attempt::Transient(error),
            Attempt::Fatal(error) => Attempt::Fatal(error),
        }
    })
}

/// One HTTP attempt, sorted for [`with_retries`]: a success, a failure worth
/// retrying, or a failure that is the answer.
fn one_attempt(agent: &ureq::Agent, request: &HttpRequest, provider: &str) -> Attempt<String> {
    let response = match request.method {
        "GET" => {
            let mut builder = agent.get(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(*name, value.as_str());
            }
            builder.call()
        }
        "POST" => {
            let mut builder = agent.post(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(*name, value.as_str());
            }
            builder.send(request.body.as_deref().unwrap_or(""))
        }
        other => {
            return Attempt::Fatal(VshotError::Translate(format!(
                "internal error: unsupported HTTP method `{other}`"
            )))
        }
    };
    // Nothing came back: the connection failed or was cut mid-flight, which a
    // fresh attempt may well fix.
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            return Attempt::Transient(VshotError::Translate(format!(
                "{provider} request failed: {error}"
            )))
        }
    };
    let status = response.status();
    let body = response.body_mut().read_to_string();
    // A definite non-retryable status is the answer even if the body never
    // arrived: the status alone says a retry will not help.
    if !status.is_success() && !retryable(status.as_u16()) {
        return Attempt::Fatal(match body {
            Ok(body) => response_error(provider, status.as_u16(), &body),
            Err(error) => {
                VshotError::Translate(format!("cannot read the {provider} response: {error}"))
            }
        });
    }
    // Everything else is either a success's body or a transport hiccup — a
    // retryable status, or a body that did not arrive intact — worth a retry.
    match body {
        Ok(body) if status.is_success() => Attempt::Done(body),
        Ok(body) => Attempt::Transient(response_error(provider, status.as_u16(), &body)),
        Err(error) => Attempt::Transient(VshotError::Translate(format!(
            "cannot read the {provider} response: {error}"
        ))),
    }
}

/// The error for a non-2xx response, quoting the service's own explanation
/// when it sent one.
///
/// The quote is cut short, and an HTML body is not quoted at all.  A throttling
/// wall answers with a whole page: pasting it in puts kilobytes of markup into
/// a message a person has to read, and into the envelope's `error` field, which
/// the editor's text layer carries for every line that failed.  Saying that a
/// page came back is the useful part; the markup is not.
fn response_error(provider: &str, status: u16, body: &str) -> VshotError {
    /// How many characters of a service's own explanation are worth quoting.
    const QUOTE_LIMIT: usize = 200;
    let detail = body.trim();
    VshotError::Translate(if detail.is_empty() {
        format!("{provider} returned HTTP {status}")
    } else if detail.starts_with('<') {
        format!(
            "{provider} returned HTTP {status} as an HTML page rather than JSON, which is how a \
             rate limit or a captcha wall answers"
        )
    } else {
        let mut quoted = String::new();
        let mut characters = detail.chars();
        for character in characters.by_ref().take(QUOTE_LIMIT) {
            // A body that spans lines would otherwise break up the message it
            // is quoted inside.
            quoted.push(if character.is_whitespace() {
                ' '
            } else {
                character
            });
        }
        if characters.next().is_some() {
            quoted.push('…');
        }
        format!("{provider} returned HTTP {status}: {quoted}")
    })
}

/// Percent-encodes a query or form value.  Everything outside the unreserved
/// set is escaped byte by byte, so a multi-byte character becomes its UTF-8
/// bytes — which is what both endpoints expect.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Requires a provider to have returned exactly one line per input line.  A
/// batch API that answers with fewer or more has lost the mapping, and there
/// is no honest way to place the lines again, so this is a provider error.
fn require_count(provider: &str, expected: usize, actual: usize) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(VshotError::Translate(format!(
        "{provider} returned {actual} lines but {expected} were expected"
    )))
}

/// Runs one request per line, up to [`GOOGLE_IN_FLIGHT`] at a time, and keeps
/// the results in the input order.
///
/// Two of the providers answer a single blob and say nothing about where one
/// input line ended and the next began, so a batch cannot be split back apart.
/// They are asked one line at a time instead and the results are placed by
/// index.  The concurrency is what keeps that from being slow: the round-trips
/// overlap without turning a free service into a target.
fn translate_per_line(
    agent: &ureq::Agent,
    provider: &str,
    lines: &[String],
    request: impl Fn(&str) -> HttpRequest + Sync,
    classify: impl Fn(&str) -> Attempt<String> + Sync,
) -> Vec<LineResult> {
    if lines.is_empty() {
        return Vec::new();
    }
    let results: Mutex<Vec<Option<LineResult>>> =
        Mutex::new((0..lines.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let workers = GOOGLE_IN_FLIGHT.min(lines.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(line) = lines.get(index) else { break };
                let outcome: LineResult = send_parsed(agent, &request(line), provider, &classify)
                    .map_err(|error| error.to_string());
                if let Some(slot) = results
                    .lock()
                    .expect("the result slots are not poisoned")
                    .get_mut(index)
                {
                    *slot = Some(outcome);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("the result slots are not poisoned")
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err("the line was never translated".into())))
        .collect()
}

/// The tokenless Google endpoint, asked once per line.
#[derive(Clone, Debug)]
pub struct GoogleEngine {
    agent: ureq::Agent,
}

impl Translator for GoogleEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        // One request per line: the endpoint cuts the text on sentence
        // boundaries and returns them as a flat list, so there is no way to
        // tell which input line a returned sentence began in.  A batch API
        // would be faster; this one cannot keep the lines apart.
        Ok(translate_per_line(
            &self.agent,
            "google",
            lines,
            |line| google_request(line, from, to),
            google_body,
        ))
    }
}

/// The request the official Google Translate Android app makes: a GET with
/// `client=at`, `dj=1` for the JSON *object* shape, and the line in the query.
///
/// The user agent is the whole reason it works — the same URL from a generic
/// agent answers with the captcha page — so it is set explicitly here, on top
/// of the shared agent's own `vshot/...`, and must not be "tidied away".
fn google_request(line: &str, from: &str, to: &str) -> HttpRequest {
    let from = Kind::Google.source_tag(from).unwrap_or_default();
    let to = Kind::Google.target_tag(to);
    let url = format!(
        "{GOOGLE_ENDPOINT}?dj=1&q={}&sl={}&tl={}&ie=UTF-8&oe=UTF-8&client=at&dt=t&otf=2",
        percent_encode(line),
        percent_encode(&from),
        percent_encode(&to),
    );
    HttpRequest {
        method: "GET",
        url,
        headers: vec![("User-Agent", GOOGLE_ANDROID_USER_AGENT.to_owned())],
        body: None,
    }
}

/// Sorts Google's body for the retry loop.
///
/// Google answers a throttled egress IP with `200 OK` and an HTML "Sorry…"
/// page rather than an HTTP error, so the status check alone never retries it.
/// A body that is not JSON at all is that page — not a shape this parser can
/// be wrong about — and is therefore transient, with a message that says what
/// it is.  A JSON body of the wrong shape *is* the service's answer and is not
/// retried.
fn google_body(body: &str) -> Attempt<String> {
    match google_parse(body) {
        Ok(text) => Attempt::Done(text),
        Err(_) if serde_json::from_str::<Value>(body).is_err() => {
            Attempt::Transient(VshotError::Translate(
                "google returned a rate-limit page instead of JSON; the network is being \
                 throttled"
                    .into(),
            ))
        }
        Err(error) => Attempt::Fatal(error),
    }
}

/// Reads Google's `dj=1` object: `{"sentences":[{"trans":"...","orig":"..."}],
/// ...}`.  The `sentences` are the pieces one input line was cut into; their
/// `trans` values are concatenated in order, which is the whole translation of
/// the one line that was sent.
fn google_parse(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| VshotError::Translate(format!("google returned invalid JSON: {error}")))?;
    let sentences = value
        .get("sentences")
        .and_then(Value::as_array)
        .ok_or_else(|| VshotError::Translate("google returned no sentence array".into()))?;
    let mut translated = String::new();
    for sentence in sentences {
        if let Some(chunk) = sentence.get("trans").and_then(Value::as_str) {
            translated.push_str(chunk);
        }
    }
    Ok(translated)
}

/// Microsoft's tokenless edge endpoint: several lines in one request.
#[derive(Clone, Debug)]
pub struct MicrosoftEngine {
    agent: ureq::Agent,
}

impl Translator for MicrosoftEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(
            &self.agent,
            &microsoft_request(from, to, lines),
            "microsoft",
        )?;
        let translated = microsoft_parse(&body)?;
        // One response object per input line: a different count has lost the
        // mapping and cannot be placed again.
        require_count("microsoft", lines.len(), translated.len())?;
        Ok(translated)
    }
}

/// The request Microsoft's edge endpoint takes: the source lines as a JSON
/// *array*, and nothing else.  There is no token and no user agent of its own
/// (the shared agent's is sent, and the endpoint ignores it); `from=` is left
/// empty for auto-detection, which is what the empty parameter is for.
fn microsoft_request(from: &str, to: &str, lines: &[String]) -> HttpRequest {
    let from = Kind::Microsoft.source_tag(from).unwrap_or_default();
    let to = Kind::Microsoft.target_tag(to);
    let url = format!(
        "{MICROSOFT_ENDPOINT}?from={}&to={}&isEnterpriseClient=false",
        percent_encode(&from),
        percent_encode(&to),
    );
    let body = serde_json::to_string(lines).expect("a list of strings always serializes");
    HttpRequest {
        method: "POST",
        url,
        headers: vec![("Content-Type", "application/json".to_owned())],
        body: Some(body),
    }
}

/// Reads `[{"translations":[{"text":"..."}]}, ...]`, one object per input line.
///
/// A line whose `translations` is empty or missing is kept as a per-line
/// failure rather than failing the batch: the array is still one entry per
/// line, so the rest of the translation is preserved and only that line is left
/// in its source language.
fn microsoft_parse(body: &str) -> Result<Vec<LineResult>> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VshotError::Translate(format!("microsoft returned invalid JSON: {error}"))
    })?;
    let entries = value
        .as_array()
        .ok_or_else(|| VshotError::Translate("microsoft returned no translation array".into()))?;
    let mut translated = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry
            .get("translations")
            .and_then(Value::as_array)
            .and_then(|translations| translations.first())
            .and_then(|translation| translation.get("text"))
            .and_then(Value::as_str);
        translated.push(match text {
            Some(text) => Ok(text.to_owned()),
            None => Err("microsoft returned a line with no translation".to_owned()),
        });
    }
    Ok(translated)
}

/// Volcengine's endpoint, asked once per line.
///
/// The API takes one `text` string and returns one `translation` string, so a
/// multi-line batch cannot be split back into the lines it came from: even
/// requiring the reply to have as many lines as the input is not enough,
/// because a translation may itself contain a newline and the boundaries can
/// shift while the count still matches.  Asking one line per request is the
/// only arrangement that cannot silently misplace text, so that is what this
/// does.
#[derive(Clone, Debug)]
pub struct VolcengineEngine {
    agent: ureq::Agent,
}

impl Translator for VolcengineEngine {
    fn translate(&self, _from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        Ok(translate_per_line(
            &self.agent,
            "volcengine",
            lines,
            |line| volcengine_request(line, to),
            volcengine_body,
        ))
    }
}

/// The request Volcengine's own Chrome extension makes.  Every header is
/// load-bearing: the `Origin` is the id of that extension, which the endpoint
/// checks, and the Chrome user agent is what it expects to see; without them
/// the request is refused.  The body carries only the target language and the
/// text — there is no source field, detection is implicit.
fn volcengine_request(line: &str, to: &str) -> HttpRequest {
    let to = Kind::Volcengine.target_tag(to);
    let body = json!({
        "target_language": to,
        "text": line,
    })
    .to_string();
    HttpRequest {
        method: "POST",
        url: VOLCENGINE_ENDPOINT.to_owned(),
        headers: vec![
            ("Content-Type", "application/json".to_owned()),
            ("Accept", "application/json, text/plain, */*".to_owned()),
            ("Origin", VOLCENGINE_ORIGIN.to_owned()),
            ("User-Agent", CHROME_USER_AGENT.to_owned()),
            ("Sec-Fetch-Site", "none".to_owned()),
            ("Sec-Fetch-Mode", "cors".to_owned()),
            ("Sec-Fetch-Dest", "empty".to_owned()),
        ],
        body: Some(body),
    }
}

/// Sorts Volcengine's body for the retry loop.  A non-zero `base_resp.status_code`
/// is the service refusing the request, which a retry will not fix, so it is
/// fatal; a transport or "try again" status is already handled upstream.
fn volcengine_body(body: &str) -> Attempt<String> {
    match volcengine_parse(body) {
        Ok(text) => Attempt::Done(text),
        Err(error) => Attempt::Fatal(error),
    }
}

/// Reads `{"translation":"...","detected_language":"...","base_resp":{...}}` and
/// turns a non-zero `base_resp.status_code` into the error it is.
fn volcengine_parse(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VshotError::Translate(format!("volcengine returned invalid JSON: {error}"))
    })?;
    if let Some(status) = value
        .get("base_resp")
        .and_then(|resp| resp.get("status_code"))
    {
        if status.as_i64() != Some(0) {
            let message = value
                .get("base_resp")
                .and_then(|resp| resp.get("status_message"))
                .and_then(Value::as_str)
                .unwrap_or("the service gave no message");
            return Err(VshotError::Translate(format!(
                "volcengine returned status {status}: {message}"
            )));
        }
    }
    value
        .get("translation")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| VshotError::Translate("volcengine returned no translation".into()))
}

/// Tencent's Transmart endpoint: several lines in one request.
#[derive(Clone, Debug)]
pub struct TransmartEngine {
    agent: ureq::Agent,
}

impl Translator for TransmartEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(
            &self.agent,
            &transmart_request(from, to, lines),
            "transmart",
        )?;
        let translated = transmart_parse(&body)?;
        // `text_list` comes back as `auto_translation`, one entry per line; a
        // different count has lost the mapping, so it is an error.
        require_count("transmart", lines.len(), translated.len())?;
        Ok(translated.into_iter().map(Ok).collect())
    }
}

/// The request Transmart's web client makes.  The `client_key` is fabricated
/// per request from a fresh UUID and the clock — it is never hashed or signed,
/// the service only checks its shape.  The source is sent exactly as asked,
/// `auto` included: the live endpoint accepts `auto` and detects the source (a
/// ja→zh request answers with `"src_lang":"ja"`), whereas the `en` some clients
/// substitute makes it echo a non-English line untranslated rather than
/// translate it.
fn transmart_request(from: &str, to: &str, lines: &[String]) -> HttpRequest {
    let from = Kind::Transmart.source_tag(from).unwrap_or_default();
    let to = Kind::Transmart.target_tag(to);
    let client_key = format!(
        "browser-chrome-120.0.0-Windows-{}-{}",
        uuid_v4(),
        epoch_millis()
    );
    let body = json!({
        "header": {
            "client_key": client_key,
            "fn": "auto_translation",
            "session": "",
            "user": "",
        },
        "source": { "lang": from, "text_list": lines },
        "target": { "lang": to },
        "model_category": "normal",
        "text_domain": "",
        "type": "plain",
    })
    .to_string();
    HttpRequest {
        method: "POST",
        url: TRANSMART_ENDPOINT.to_owned(),
        headers: vec![
            ("Content-Type", "application/json; charset=UTF-8".to_owned()),
            ("User-Agent", CHROME_USER_AGENT.to_owned()),
        ],
        body: Some(body),
    }
}

/// A fresh v4-shaped UUID for Transmart's `client_key`.
///
/// Nothing checks that it is a real UUID — the field is fabricated per request
/// and only its shape matters — so this is a small xorshift PRNG seeded from
/// the clock and a counter rather than a cryptographic generator, which would
/// mean another dependency for nothing.
fn uuid_v4() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    // The constant keeps the seed non-zero even if the clock reads zero, which
    // xorshift needs to make any progress at all.
    let mut state = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
        ^ 0xD1B5_4A32_D192_ED03;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&next().to_le_bytes());
    bytes[8..].copy_from_slice(&next().to_le_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Milliseconds since the Unix epoch, for the same fabricated `client_key`.
fn epoch_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Reads `{"header":{"ret_code":"succ"},"auto_translation":[...],"src_lang":
/// "..."}`.  `ret_code` must be `succ`, and each line has its trailing
/// whitespace trimmed, which is what the client this recipe came from does.
fn transmart_parse(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VshotError::Translate(format!("transmart returned invalid JSON: {error}"))
    })?;
    let ret_code = value
        .get("header")
        .and_then(|header| header.get("ret_code"))
        .and_then(Value::as_str)
        .ok_or_else(|| VshotError::Translate("transmart returned no ret_code".into()))?;
    if ret_code != "succ" {
        return Err(VshotError::Translate(format!(
            "transmart returned {ret_code} rather than succ"
        )));
    }
    let entries = value
        .get("auto_translation")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            VshotError::Translate("transmart returned no auto_translation array".into())
        })?;
    let mut translated = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry.as_str().ok_or_else(|| {
            VshotError::Translate("transmart returned a non-string translation".into())
        })?;
        translated.push(text.trim_end().to_owned());
    }
    Ok(translated)
}

/// Caiyun's interpreter endpoint (`lingocloud`): several lines in one request.
#[derive(Clone, Debug)]
pub struct LingocloudEngine {
    agent: ureq::Agent,
    token: String,
}

impl Translator for LingocloudEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(
            &self.agent,
            &lingocloud_request(self, from, to, lines),
            "lingocloud",
        )?;
        let translated = lingocloud_parse(&body)?;
        // `target` comes back as one entry per input line, in order; a
        // different count has lost the mapping, so it is refused before it is
        // accepted rather than misplacing text.
        require_count("lingocloud", lines.len(), translated.len())?;
        Ok(translated.into_iter().map(Ok).collect())
    }
}

/// The request Caiyun's web app makes.  All three headers are required, and
/// the source lines are the body's `source` array.  The pair is spliced from
/// the two tags — `auto2zh` for a detected source, `ja2de` for a named one —
/// and `detect:true` asks the endpoint to resolve an `auto` source itself.
/// `request_id` is the epoch in milliseconds, sent as a string.
fn lingocloud_request(
    engine: &LingocloudEngine,
    from: &str,
    to: &str,
    lines: &[String],
) -> HttpRequest {
    let from = Kind::Lingocloud.source_tag(from).unwrap_or_default();
    let to = Kind::Lingocloud.target_tag(to);
    let body = json!({
        "source": lines,
        "trans_type": format!("{from}2{to}"),
        "request_id": epoch_millis().to_string(),
        "detect": true,
    })
    .to_string();
    HttpRequest {
        method: "POST",
        url: LINGOCLOUD_ENDPOINT.to_owned(),
        headers: vec![
            ("Content-Type", "application/json; charset=UTF-8".to_owned()),
            ("X-Authorization", format!("token {}", engine.token)),
            ("User-Agent", LINGOCLOUD_USER_AGENT.to_owned()),
        ],
        body: Some(body),
    }
}

/// Reads `{"rc":0,"target":[...]}`.  A non-zero `rc` is the service saying no,
/// and its `message` is a real explanation — `Unsupported trans_type` for a
/// pair it will not take — so that is surfaced rather than the whole object.
/// `target` is one string per input line, in order.
fn lingocloud_parse(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        VshotError::Translate(format!("lingocloud returned invalid JSON: {error}"))
    })?;
    let rc = value.get("rc").and_then(Value::as_i64);
    if rc != Some(0) {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the service gave no message");
        let rc = rc
            .map(|rc| rc.to_string())
            .unwrap_or_else(|| "absent".to_owned());
        return Err(VshotError::Translate(format!(
            "lingocloud returned rc {rc}: {message}"
        )));
    }
    let entries = value
        .get("target")
        .and_then(Value::as_array)
        .ok_or_else(|| VshotError::Translate("lingocloud returned no target array".into()))?;
    let mut translated = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry.as_str().ok_or_else(|| {
            VshotError::Translate("lingocloud returned a non-string translation".into())
        })?;
        translated.push(text.to_owned());
    }
    Ok(translated)
}

/// Azure Translator: several lines in one authenticated request.
#[derive(Clone, Debug)]
pub struct BingEngine {
    agent: ureq::Agent,
    endpoint: String,
    api_key: String,
    region: Option<String>,
}

impl Translator for BingEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(&self.agent, &bing_request(self, from, to, lines), "bing")?;
        let translated = bing_parse(&body)?;
        require_count("bing", lines.len(), translated.len())?;
        Ok(translated.into_iter().map(Ok).collect())
    }
}

fn bing_request(engine: &BingEngine, from: &str, to: &str, lines: &[String]) -> HttpRequest {
    let mut url = format!(
        "{}/translate?api-version=3.0",
        engine.endpoint.trim_end_matches('/')
    );
    // `auto` is Bing's spelling of "detect": it takes no `from` at all.
    if let Some(from) = Kind::Bing.source_tag(from) {
        url.push_str(&format!("&from={}", percent_encode(&from)));
    }
    url.push_str(&format!(
        "&to={}",
        percent_encode(&Kind::Bing.target_tag(to))
    ));

    let entries: Vec<Value> = lines.iter().map(|text| json!({ "Text": text })).collect();
    let body = serde_json::to_string(&entries)
        .expect("a list of {\"Text\": ...} objects always serializes");

    let mut headers = vec![
        ("Ocp-Apim-Subscription-Key", engine.api_key.clone()),
        ("Content-Type", "application/json".to_owned()),
    ];
    if let Some(region) = &engine.region {
        headers.push(("Ocp-Apim-Subscription-Region", region.clone()));
    }
    HttpRequest {
        method: "POST",
        url,
        headers,
        body: Some(body),
    }
}

/// Reads `[{"translations":[{"text":"..."}]}]`, or surfaces the API's own
/// error object.
fn bing_parse(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| VshotError::Translate(format!("bing returned invalid JSON: {error}")))?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the service gave no message");
        return Err(VshotError::Translate(format!(
            "bing returned an error: {message}"
        )));
    }
    let entries = value
        .as_array()
        .ok_or_else(|| VshotError::Translate("bing returned no translation array".into()))?;
    let mut translated = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry
            .get("translations")
            .and_then(Value::as_array)
            .and_then(|translations| translations.first())
            .and_then(|translation| translation.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                VshotError::Translate("bing returned an entry with no translation".into())
            })?;
        translated.push(text.to_owned());
    }
    Ok(translated)
}

/// Baidu's fanyi API: the lines are joined into one `q`, signed with an MD5
/// digest of the credentials.
#[derive(Clone, Debug)]
pub struct BaiduEngine {
    agent: ureq::Agent,
    app_id: String,
    secret_key: String,
}

impl Translator for BaiduEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(
            &self.agent,
            &baidu_request(self, from, to, lines, &salt()),
            "baidu",
        )?;
        let translated = baidu_parse(&body)?;
        // Baidu answers one `trans_result` entry per line of the one `q`; a
        // different count means the split is wrong and the lines cannot be
        // placed again, so it is an error rather than a partial answer.
        require_count("baidu", lines.len(), translated.len())?;
        Ok(translated.into_iter().map(Ok).collect())
    }
}

/// A nonce for one request.  Baidu only requires the salt to be unique enough
/// that two requests cannot be replayed for one another; a nanosecond
/// timestamp plus a counter is that without pulling in a random generator.
fn salt() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}{count}")
}

/// The digest Baidu checks: MD5 of `appid + q + salt + secret_key`, in that
/// order, with no separator.  It is pinned by a test with a known answer
/// because a future refactor that quietly reorders the parts would otherwise
/// only fail against the live service.
fn baidu_sign(app_id: &str, q: &str, salt: &str, secret_key: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(format!("{app_id}{q}{salt}{secret_key}"));
    format!("{:x}", hasher.finalize())
}

fn baidu_request(
    engine: &BaiduEngine,
    from: &str,
    to: &str,
    lines: &[String],
    salt: &str,
) -> HttpRequest {
    // One `q` holding every line, newline-separated: that is how Baidu returns
    // one `trans_result` entry per line.
    let q = lines.join("\n");
    let from = Kind::Baidu.source_tag(from).unwrap_or_default();
    let to = Kind::Baidu.target_tag(to);
    let sign = baidu_sign(&engine.app_id, &q, salt, &engine.secret_key);
    let body = format!(
        "q={}&from={}&to={}&appid={}&salt={}&sign={sign}",
        percent_encode(&q),
        percent_encode(&from),
        percent_encode(&to),
        percent_encode(&engine.app_id),
        percent_encode(salt),
    );
    HttpRequest {
        method: "POST",
        url: BAIDU_ENDPOINT.to_owned(),
        headers: vec![(
            "Content-Type",
            "application/x-www-form-urlencoded".to_owned(),
        )],
        body: Some(body),
    }
}

/// Reads `{"trans_result":[{"src":"..","dst":".."}]}`, treating a non-empty
/// `error_code` as the failure the API says it is.
fn baidu_parse(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| VshotError::Translate(format!("baidu returned invalid JSON: {error}")))?;
    if let Some(code) = value.get("error_code") {
        let ok = code.is_null() || code.as_str() == Some("0") || code.as_i64() == Some(0);
        if !ok {
            let message = value
                .get("error_msg")
                .and_then(Value::as_str)
                .unwrap_or("the service gave no message");
            return Err(VshotError::Translate(format!(
                "baidu returned error {code}: {message}"
            )));
        }
    }
    let entries = value
        .get("trans_result")
        .and_then(Value::as_array)
        .ok_or_else(|| VshotError::Translate("baidu returned no trans_result".into()))?;
    let mut translated = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry
            .get("dst")
            .and_then(Value::as_str)
            .ok_or_else(|| VshotError::Translate("baidu returned a result with no dst".into()))?;
        translated.push(text.to_owned());
    }
    Ok(translated)
}

/// Any OpenAI-compatible chat-completions endpoint.
#[derive(Clone, Debug)]
pub struct AiEngine {
    agent: ureq::Agent,
    endpoint: String,
    api_key: String,
    model: String,
    /// The system prompt, or empty to use the built-in translation prompt.
    prompt: String,
}

impl Translator for AiEngine {
    fn translate(&self, from: &str, to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let body = send(&self.agent, &ai_request(self, from, to, lines), "ai")?;
        let content = ai_parse(&body)?;
        let translated = ai_reply_lines(&content, lines.len())?;
        Ok(translated.into_iter().map(Ok).collect())
    }
}

/// The built-in system prompt, used when `translate.ai.prompt` is empty.  It
/// is explicit about the reply being *only* a JSON array of strings, one per
/// input line, because that is the one shape this side can place.
fn default_ai_prompt(from: &str, to: &str) -> String {
    let source = if from == "auto" {
        "the source language (detect it yourself)".to_owned()
    } else {
        from.to_owned()
    };
    format!(
        "You are a translation engine. Translate every line of the user's message from {source} \
         into {to}. Reply with only a JSON array of strings, one translated line per input line, \
         in the same order, and nothing else: no commentary, no explanation, no markdown fence."
    )
}

fn ai_request(engine: &AiEngine, from: &str, to: &str, lines: &[String]) -> HttpRequest {
    // An endpoint that already names the path is used verbatim; otherwise the
    // OpenAI convention is `<base>/chat/completions`.
    let url = if engine.endpoint.ends_with("/chat/completions") {
        engine.endpoint.clone()
    } else {
        format!("{}/chat/completions", engine.endpoint.trim_end_matches('/'))
    };
    let system = if engine.prompt.trim().is_empty() {
        default_ai_prompt(from, to)
    } else {
        engine.prompt.clone()
    };
    let body = json!({
        "model": engine.model,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": lines.join("\n") },
        ],
    })
    .to_string();
    HttpRequest {
        method: "POST",
        url,
        headers: vec![
            ("Authorization", format!("Bearer {}", engine.api_key)),
            ("Content-Type", "application/json".to_owned()),
        ],
        body: Some(body),
    }
}

/// Pulls the assistant's text out of a chat-completions response.
fn ai_parse(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| VshotError::Translate(format!("ai returned invalid JSON: {error}")))?;
    // An error object is common enough to name specially.
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the service gave no message");
        return Err(VshotError::Translate(format!(
            "ai returned an error: {message}"
        )));
    }
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| VshotError::Translate("ai returned no message content".into()))
}

/// Turns the assistant's reply into exactly `expected` lines.
///
/// The prompt asks for a JSON array, but a model may still fence it or answer
/// with one line per line; both are read.  A count that does not match is an
/// error, because a reply of the wrong length cannot be placed without
/// silently dropping or inventing a line.
fn ai_reply_lines(reply: &str, expected: usize) -> Result<Vec<String>> {
    let stripped = strip_fence(reply);
    if let Ok(values) = serde_json::from_str::<Vec<String>>(stripped.trim()) {
        if values.len() != expected {
            return Err(VshotError::Translate(format!(
                "ai returned {} lines but {expected} were expected",
                values.len()
            )));
        }
        return Ok(values);
    }
    // Fallback: one line per line, with any fence line dropped.
    let lines: Vec<String> = stripped
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("```"))
        .map(str::to_owned)
        .collect();
    if lines.len() != expected {
        return Err(VshotError::Translate(format!(
            "ai returned {} lines but {expected} were expected",
            lines.len()
        )));
    }
    Ok(lines)
}

/// Removes a ```-fence around a reply, if the model added one.  The opening
/// line may carry a language tag (`json`); the closing fence is dropped too.
fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = match rest.find('\n') {
        Some(newline) => &rest[newline + 1..],
        None => "",
    };
    let body = body.trim_end();
    body.strip_suffix("```").unwrap_or(body).trim_end()
}

/// A command of the user's own, run as an argv array: source lines on stdin,
/// translated lines on stdout.  This is the escape hatch, so its error
/// messages spell out exactly what went wrong and what the contract is.
#[derive(Clone, Debug)]
pub struct ExternalEngine {
    command: Vec<String>,
    timeout: Duration,
}

impl Translator for ExternalEngine {
    fn translate(&self, _from: &str, _to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
        let program = self.command.first().ok_or_else(|| {
            VshotError::Translate("the external translation command is empty".into())
        })?;
        let mut child = Command::new(program)
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                VshotError::Translate(format!(
                    "cannot run the external translation command `{program}`: {error}"
                ))
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            let mut input = lines.join("\n");
            if !input.is_empty() {
                input.push('\n');
            }
            // A program that exits without reading all of stdin closes the
            // pipe; that is its business, not an error here.
            let _ = stdin.write_all(input.as_bytes());
        }

        // `wait_with_output` has no timeout, so the wait is done by hand with
        // the deadline checked between polls: a program that hangs must not
        // hang the translation that asked for it.
        let deadline = std::time::Instant::now() + self.timeout;
        let output = loop {
            match child.try_wait() {
                Ok(Some(_)) => break child.wait_with_output(),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(VshotError::Translate(format!(
                            "the external translation command `{program}` did not finish within \
                             {} seconds",
                            self.timeout.as_secs()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(VshotError::Translate(format!(
                        "cannot wait for the external translation command `{program}`: {error}"
                    )))
                }
            }
        };
        let output = output.map_err(|error| {
            VshotError::Translate(format!("cannot read the output of `{program}`: {error}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            return Err(VshotError::Translate(if stderr.is_empty() {
                format!(
                    "the external translation command `{program}` exited with {}",
                    output.status
                )
            } else {
                format!(
                    "the external translation command `{program}` exited with {}: {stderr}",
                    output.status
                )
            }));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let translated: Vec<String> = text.lines().map(str::to_owned).collect();
        if translated.len() != lines.len() {
            return Err(VshotError::Translate(format!(
                "the external translation command `{program}` printed {} lines but {} were sent; \
                 it must print exactly one translated line per input line, in order (see the \
                 README's Translation section)",
                translated.len(),
                lines.len()
            )));
        }
        Ok(translated.into_iter().map(Ok).collect())
    }
}

/// A translated OCR envelope and the same lines as plain text.
#[derive(Debug)]
pub struct Translated {
    /// The envelope to feed a JSON consumer: the OCR envelope with `lines`
    /// rewritten and `provider`/`from`/`to` added.
    pub envelope: String,
    /// The translated lines joined with `\n`, a failed line falling back to
    /// its source.  No trailing newline; the caller adds one for stdout.
    pub text: String,
    /// How many lines fell back to their source text.
    failed_lines: usize,
    /// How many lines there were in all.
    total_lines: usize,
    /// The first failed line's reason, for [`Translated::warn_left_in_source`];
    /// `None` when every line translated.
    first_failure: Option<String>,
}

impl Translated {
    /// Says on stderr how many lines were left in the source language.
    ///
    /// A failed line keeps its source text and is otherwise indistinguishable
    /// from a translation, so the routes that print the text say so here.  The
    /// `--stdin-ocr` route never calls this: its consumer reads the per-line
    /// `error` field out of the envelope instead.
    pub fn warn_left_in_source(&self) {
        let Some(first) = &self.first_failure else {
            return;
        };
        eprintln!(
            "vshot: {} of {} lines were left in the original language (first failure: {first})",
            self.failed_lines, self.total_lines
        );
    }
}

/// Translates a batch through an ordered chain of providers.
///
/// Every provider answers the same question, so the first one that produces a
/// usable result is the answer and the rest are not asked.  A result is usable
/// when **at least one line translated**: the per-line providers report a
/// throttled batch as a full set of per-line errors rather than a batch `Err`,
/// so a chain that reacted only to `Err` would never fire on the case it
/// exists for.  A usable result is kept as-is, per-line failures included, and
/// its provider name is returned so the envelope can report the one that
/// actually ran.  When a chain of two or more is exhausted, the error names
/// every provider tried and the last reason each one gave.
///
/// A chain of one is not a chain: its result is the answer exactly as it was
/// before `fallback` existed — even a batch of nothing but per-line failures,
/// which the Qt overlay reads line by line rather than as a batch error.  With
/// no `fallback` configured this is the whole of today's behaviour, which the
/// empty default promises to leave alone.
fn translate_chain(
    providers: &[(&str, &dyn Translator)],
    from: &str,
    to: &str,
    lines: &[String],
) -> Result<(String, Vec<LineResult>)> {
    // An empty batch has nothing to translate, so no provider would count as
    // having produced a line; answer it with the first provider's name so the
    // envelope still says who would have run.
    if lines.is_empty() {
        return match providers.first() {
            Some((name, _)) => Ok(((*name).to_owned(), Vec::new())),
            None => Err(VshotError::Translate(
                "no translate provider is configured".into(),
            )),
        };
    }
    if let [(name, translator)] = providers {
        let results = translator.translate(from, to, lines)?;
        return Ok(((*name).to_owned(), results));
    }
    let mut tried: Vec<(String, String)> = Vec::new();
    for (name, translator) in providers {
        match translator.translate(from, to, lines) {
            Ok(results) if results.iter().any(LineResult::is_ok) => {
                return Ok(((*name).to_owned(), results));
            }
            Ok(results) => {
                let reason = results
                    .iter()
                    .rev()
                    .find_map(|line| line.as_ref().err().cloned())
                    .unwrap_or_else(|| "every line failed".to_owned());
                tried.push(((*name).to_owned(), reason));
            }
            Err(error) => tried.push(((*name).to_owned(), error.to_string())),
        }
    }
    let detail = tried
        .iter()
        .map(|(name, reason)| format!("{name}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(VshotError::Translate(format!(
        "every translate provider failed: {detail}"
    )))
}

/// Translates the lines of an OCR envelope through a chain of providers and
/// returns both the translated envelope and the plain text.
///
/// This is the whole of the `--stdin-ocr` route: the envelope `vshot ocr
/// --json` prints goes in, its `lines[].text` are translated, and the result
/// comes out in the same shape.  No compositor, no capture, no OCR run — which
/// is what makes it fast enough for the editor to call while its window is up.
/// The envelope's `provider` reports the provider that answered, not the one
/// that was asked for first.
pub fn translate_envelope_chain(
    envelope: &str,
    providers: &[(&str, &dyn Translator)],
    from: &str,
    to: &str,
) -> Result<Translated> {
    let parsed: Value = serde_json::from_str(envelope).map_err(|error| {
        VshotError::Translate(format!("cannot parse the OCR JSON on stdin: {error}"))
    })?;
    let lines = parsed
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            VshotError::Translate("the OCR JSON on stdin has no `lines` array".into())
        })?;
    let sources: Vec<String> = lines
        .iter()
        .map(|line| {
            line.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let (provider, translations) = translate_chain(providers, from, to, &sources)?;
    if translations.len() != sources.len() {
        return Err(VshotError::Translate(format!(
            "the provider returned {} lines for {} inputs",
            translations.len(),
            sources.len()
        )));
    }
    let text = sources
        .iter()
        .zip(&translations)
        .map(|(source, translation)| match translation {
            Ok(text) => text.clone(),
            Err(_) => source.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let failed_lines = translations
        .iter()
        .filter(|translation| translation.is_err())
        .count();
    let first_failure = translations
        .iter()
        .find_map(|translation| translation.as_ref().err().cloned());
    let envelope = build_envelope(&parsed, lines, &sources, &translations, &provider, from, to);
    Ok(Translated {
        envelope,
        text,
        failed_lines,
        total_lines: sources.len(),
        first_failure,
    })
}

fn build_envelope(
    input: &Value,
    lines: &[Value],
    sources: &[String],
    translations: &[LineResult],
    provider: &str,
    from: &str,
    to: &str,
) -> String {
    let geometry = input
        .get("geometry")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let version = input.get("version").and_then(Value::as_u64).unwrap_or(1);
    let out_lines: Vec<Value> = lines
        .iter()
        .zip(sources)
        .zip(translations)
        .map(|((line, source), translation)| {
            let mut object = serde_json::Map::new();
            match translation {
                Ok(text) => {
                    object.insert("text".to_owned(), json!(text));
                }
                Err(message) => {
                    object.insert("text".to_owned(), json!(source));
                    object.insert("error".to_owned(), json!(message));
                }
            }
            object.insert("source".to_owned(), json!(source));
            // Keep the line's position so the Qt text layer can place the
            // translation; drop the per-character boxes, which are only useful
            // for selecting the source text.
            if geometry {
                if let Some(rect) = line.get("rect") {
                    object.insert("rect".to_owned(), rect.clone());
                }
            }
            Value::Object(object)
        })
        .collect();
    json!({
        "version": version,
        "geometry": geometry,
        "provider": provider,
        "from": from,
        "to": to,
        "lines": out_lines,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> ureq::Agent {
        agent(Duration::from_secs(1))
    }

    /// The single-provider shape the envelope tests use; production goes
    /// through the chain entry point, which is all a resolved config needs.
    fn translate_envelope(
        envelope: &str,
        translator: &dyn Translator,
        provider: &str,
        from: &str,
        to: &str,
    ) -> Result<Translated> {
        translate_envelope_chain(envelope, &[(provider, translator)], from, to)
    }

    #[test]
    fn each_provider_spells_languages_its_own_way() {
        // Google takes `zh-CN`/`zh-TW` and spells detection `auto`.
        assert_eq!(Kind::Google.target_tag("zh-Hans"), "zh-CN");
        assert_eq!(Kind::Google.target_tag("zh-Hant"), "zh-TW");
        assert_eq!(Kind::Google.source_tag("auto").as_deref(), Some("auto"));
        assert_eq!(Kind::Google.target_tag("ja"), "ja");

        // Microsoft takes vshot's tags as they are; detection is the empty
        // `from` (a missing source tag).
        assert_eq!(Kind::Microsoft.target_tag("zh-Hans"), "zh-Hans");
        assert_eq!(Kind::Microsoft.target_tag("ja"), "ja");
        assert_eq!(Kind::Microsoft.source_tag("auto"), None);
        assert_eq!(Kind::Microsoft.source_tag("ja").as_deref(), Some("ja"));

        // Volcengine takes most tags as they are, but spells simplified
        // Chinese `zh`: `zh-Hans` makes the live endpoint answer in English.
        // It never carries a source at all: detection is always implicit.
        assert_eq!(Kind::Volcengine.target_tag("zh-Hans"), "zh");
        assert_eq!(Kind::Volcengine.target_tag("zh-Hant"), "zh-Hant");
        assert_eq!(Kind::Volcengine.target_tag("ja"), "ja");
        assert_eq!(Kind::Volcengine.source_tag("ja"), None);

        // Transmart collapses both Chinese scripts to `zh`; its source is
        // passed through as asked, `auto` included, because the live service
        // accepts `auto` and detects the source.
        assert_eq!(Kind::Transmart.target_tag("zh-Hans"), "zh");
        assert_eq!(Kind::Transmart.target_tag("zh-Hant"), "zh");
        assert_eq!(Kind::Transmart.target_tag("ja"), "ja");
        assert_eq!(Kind::Transmart.source_tag("auto").as_deref(), Some("auto"));
        assert_eq!(Kind::Transmart.source_tag("ja").as_deref(), Some("ja"));

        // Caiyun's `lingocloud` splices `<from>2<to>`, and only `zh-Hans` is
        // rewritten: the endpoint rejects `zh-Hans` on either side of the pair
        // (`rc=-1 Unsupported trans_type`), while `zh-Hant` is taken as written
        // and really does yield traditional Chinese as a target.  A language the
        // old six-language client would have refused (`de`) passes straight
        // through, `auto` included.
        assert_eq!(Kind::Lingocloud.target_tag("zh-Hans"), "zh");
        assert_eq!(Kind::Lingocloud.target_tag("zh-Hant"), "zh-Hant");
        assert_eq!(Kind::Lingocloud.target_tag("de"), "de");
        assert_eq!(Kind::Lingocloud.source_tag("auto").as_deref(), Some("auto"));
        assert_eq!(
            Kind::Lingocloud.source_tag("zh-Hans").as_deref(),
            Some("zh")
        );
        assert_eq!(
            Kind::Lingocloud.source_tag("zh-Hant").as_deref(),
            Some("zh-Hant")
        );

        // Bing is the way vshot writes it, and detection is the absence of a
        // `from` parameter entirely.
        assert_eq!(Kind::Bing.source_tag("auto"), None);
        assert_eq!(Kind::Bing.source_tag("zh-Hans").as_deref(), Some("zh-Hans"));

        // Baidu has codes of its own.
        assert_eq!(Kind::Baidu.target_tag("zh-Hans"), "zh");
        assert_eq!(Kind::Baidu.target_tag("zh-Hant"), "cht");
        assert_eq!(Kind::Baidu.target_tag("ja"), "jp");
        assert_eq!(Kind::Baidu.target_tag("ko"), "kor");
        assert_eq!(Kind::Baidu.target_tag("fr"), "fra");
        assert_eq!(Kind::Baidu.source_tag("auto").as_deref(), Some("auto"));

        // The AI and external escape hatches take vshot's own spelling
        // through, and so does an unknown tag everywhere: never dropped.
        for kind in [
            Kind::Google,
            Kind::Microsoft,
            Kind::Volcengine,
            Kind::Transmart,
            Kind::Lingocloud,
            Kind::Bing,
            Kind::Baidu,
            Kind::Ai,
            Kind::External,
        ] {
            assert_eq!(kind.target_tag("tlh"), "tlh", "{kind:?}");
        }
        for kind in [Kind::Ai, Kind::External] {
            assert_eq!(kind.target_tag("zh-Hans"), "zh-Hans", "{kind:?}");
            assert_eq!(kind.source_tag("auto").as_deref(), Some("auto"), "{kind:?}");
        }
    }

    #[test]
    fn the_provider_name_is_read_and_an_unknown_one_is_reported() {
        assert_eq!(Kind::parse("google").unwrap(), Kind::Google);
        assert_eq!(Kind::parse("microsoft").unwrap(), Kind::Microsoft);
        assert_eq!(Kind::parse("volcengine").unwrap(), Kind::Volcengine);
        assert_eq!(Kind::parse("transmart").unwrap(), Kind::Transmart);
        assert_eq!(Kind::parse("lingocloud").unwrap(), Kind::Lingocloud);
        assert_eq!(Kind::parse("bing").unwrap(), Kind::Bing);
        assert_eq!(Kind::parse("baidu").unwrap(), Kind::Baidu);
        assert_eq!(Kind::parse("ai").unwrap(), Kind::Ai);
        assert_eq!(Kind::parse("external").unwrap(), Kind::External);
        let error = Kind::parse("deepl").unwrap_err();
        assert!(error.to_string().contains("deepl"), "{error}");
    }

    #[test]
    fn a_provider_that_needs_credentials_says_so() {
        use crate::config::{TranslateAiDefaults, TranslateBingDefaults};
        let empty = TranslateDefaults::default();
        // The keyless providers need nothing, so a bare config resolves them.
        assert!(matches!(
            engine_from_config(&empty, "google").unwrap(),
            Engine::Google(_)
        ));
        assert!(matches!(
            engine_from_config(&empty, "microsoft").unwrap(),
            Engine::Microsoft(_)
        ));
        assert!(matches!(
            engine_from_config(&empty, "volcengine").unwrap(),
            Engine::Volcengine(_)
        ));
        assert!(matches!(
            engine_from_config(&empty, "transmart").unwrap(),
            Engine::Transmart(_)
        ));
        // `lingocloud` borrows a built-in token, so a bare config resolves it
        // too: it is the one credential-free-by-config provider with an
        // optional override rather than a required key.
        assert!(matches!(
            engine_from_config(&empty, "lingocloud").unwrap(),
            Engine::Lingocloud(_)
        ));
        // Each of the keyed ones names the key it is missing.
        for (provider, needle) in [("bing", "api-key"), ("baidu", "app-id"), ("ai", "endpoint")] {
            let error = engine_from_config(&empty, provider).unwrap_err();
            assert!(error.to_string().contains(needle), "{provider}: {error}");
        }
        // A configured key resolves.
        let bing = TranslateDefaults {
            bing: Some(TranslateBingDefaults {
                api_key: Some("k".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            engine_from_config(&bing, "bing").unwrap(),
            Engine::Bing(_)
        ));
        let ai = TranslateDefaults {
            ai: Some(TranslateAiDefaults {
                endpoint: Some("https://example.com/v1".to_owned()),
                api_key: Some("k".to_owned()),
                model: Some("m".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            engine_from_config(&ai, "ai").unwrap(),
            Engine::Ai(_)
        ));
        // An external provider with no command is refused by name.
        let error = engine_from_config(&empty, "external").unwrap_err();
        assert!(error.to_string().contains("external.command"), "{error}");
    }

    #[test]
    fn google_builds_the_android_app_request() {
        let request = google_request("hello world", "en", "zh-Hans");
        assert_eq!(request.method, "GET");
        // `dj=1` asks for the object shape, `client=at` is the Android app.
        assert_eq!(
            request.url,
            "https://translate.google.com/translate_a/single?dj=1&q=hello%20world&sl=en&tl=zh-CN&ie=UTF-8&oe=UTF-8&client=at&dt=t&otf=2"
        );
        assert_eq!(request.body, None);
        // The user agent is the recipe: without it the endpoint answers with a
        // captcha page instead of JSON, so dropping it must fail here.
        assert!(request
            .headers
            .contains(&("User-Agent", GOOGLE_ANDROID_USER_AGENT.to_owned())));
    }

    #[test]
    fn google_reads_the_dj_object_sentences() {
        let body = r#"{"sentences":[{"trans":"你好","orig":"hello","backend":10}],"src":"en","confidence":1.0}"#;
        assert_eq!(google_parse(body).unwrap(), "你好");
        // The sentences are concatenated in order.
        let split = r#"{"sentences":[{"trans":"你好","orig":"hello"},{"trans":"世界","orig":"world"}],"src":"en"}"#;
        assert_eq!(google_parse(split).unwrap(), "你好世界");
    }

    #[test]
    fn microsoft_builds_the_array_body() {
        let request = microsoft_request(
            "auto",
            "zh-Hans",
            &["こんにちは、世界".to_owned(), "さようなら".to_owned()],
        );
        assert_eq!(request.method, "POST");
        // `auto` leaves `from` empty, which is how this endpoint detects.
        assert_eq!(
            request.url,
            "https://edge.microsoft.com/translate/translatetext?from=&to=zh-Hans&isEnterpriseClient=false"
        );
        // The source lines themselves are the body, so a batch is one request.
        assert_eq!(
            request.body.as_deref(),
            Some(r#"["こんにちは、世界","さようなら"]"#)
        );
        // Content-Type and nothing else: no token, no user agent of its own.
        assert_eq!(
            request.headers,
            vec![("Content-Type", "application/json".to_owned())]
        );
    }

    #[test]
    fn microsoft_reads_the_array_response_and_survives_an_empty_line() {
        let body = r#"[{"detectedLanguage":{"language":"ja","score":1.0},"translations":[{"text":"你好，世界","to":"zh-Hans"}]},{"translations":[{"text":"再见","to":"zh-Hans"}]}]"#;
        let parsed = microsoft_parse(body).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_deref().unwrap(), "你好，世界");
        assert_eq!(parsed[1].as_deref().unwrap(), "再见");

        // A line whose `translations` is empty is a per-line failure, not a
        // batch one: the rest of the batch is kept.
        let body = r#"[{"translations":[{"text":"甲"}]},{"translations":[]},{"translations":[{"text":"丙"}]}]"#;
        let parsed = microsoft_parse(body).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].as_deref().unwrap(), "甲");
        assert!(parsed[1].is_err(), "{:?}", parsed[1]);
        assert_eq!(parsed[2].as_deref().unwrap(), "丙");
    }

    #[test]
    fn volcengine_sends_every_header_it_checks() {
        let request = volcengine_request("こんにちは、世界", "zh-Hans");
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, VOLCENGINE_ENDPOINT);
        // The Origin is the vendor's own extension id, which the endpoint
        // checks; the Chrome user agent is what it expects to see.  Every one
        // of these is load-bearing.
        assert!(request.headers.contains(&(
            "Origin",
            "chrome-extension://klgfhbiooeogfpknjdcbablpceialkdj".to_owned()
        )));
        assert!(request
            .headers
            .contains(&("User-Agent", CHROME_USER_AGENT.to_owned())));
        assert!(request
            .headers
            .contains(&("Content-Type", "application/json".to_owned())));
        assert!(request
            .headers
            .contains(&("Accept", "application/json, text/plain, */*".to_owned())));
        assert!(request
            .headers
            .contains(&("Sec-Fetch-Site", "none".to_owned())));
        assert!(request
            .headers
            .contains(&("Sec-Fetch-Mode", "cors".to_owned())));
        assert!(request
            .headers
            .contains(&("Sec-Fetch-Dest", "empty".to_owned())));
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        // Simplified Chinese goes out as `zh`, the code the live endpoint takes.
        assert_eq!(body["target_language"], "zh");
        assert_eq!(body["text"], "こんにちは、世界");
        // No source field: detection is implicit.
        assert!(body.get("source").is_none());
    }

    #[test]
    fn volcengine_reads_the_translation_and_rejects_a_bad_status() {
        let body = r#"{"translation":"你好，世界","detected_language":"ja","probability":1,"base_resp":{"status_code":0,"status_message":""}}"#;
        assert_eq!(volcengine_parse(body).unwrap(), "你好，世界");

        // A non-zero status is the service refusing the request, and the retry
        // loop must see it as fatal, not as a hiccup.
        let bad = r#"{"translation":"","detected_language":"ja","base_resp":{"status_code":1,"status_message":"invalid request"}}"#;
        assert!(matches!(volcengine_body(bad), Attempt::Fatal(_)));
        let error = volcengine_parse(bad).unwrap_err();
        assert!(error.to_string().contains("invalid request"), "{error}");
        assert!(error.to_string().contains('1'), "{error}");
    }

    #[test]
    fn transmart_fabricates_a_well_formed_client_key() {
        let request = transmart_request(
            "auto",
            "zh-Hans",
            &["ライン1".to_owned(), "ライン2".to_owned()],
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, TRANSMART_ENDPOINT);
        assert!(request
            .headers
            .contains(&("Content-Type", "application/json; charset=UTF-8".to_owned())));
        assert!(request
            .headers
            .contains(&("User-Agent", CHROME_USER_AGENT.to_owned())));
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        // The source is passed through as `auto`, which the live endpoint
        // detects; both Chinese scripts collapse to `zh`.
        assert_eq!(body["source"]["lang"], "auto");
        assert_eq!(body["target"]["lang"], "zh");
        // The lines batch in `text_list`.
        assert_eq!(body["source"]["text_list"][0], "ライン1");
        assert_eq!(body["source"]["text_list"][1], "ライン2");
        assert_eq!(body["header"]["fn"], "auto_translation");
        let client_key = body["header"]["client_key"].as_str().unwrap();
        assert_client_key_shape(client_key);
    }

    /// Asserts the `browser-chrome-120.0.0-Windows-<uuid>-<digits>` shape
    /// without pulling in a regex crate: the uuid is 36 characters in the
    /// 8-4-4-4-12 layout with a `4` version and an `8`/`9`/`a`/`b` variant, and
    /// the tail is all digits.
    fn assert_client_key_shape(client_key: &str) {
        let rest = client_key
            .strip_prefix("browser-chrome-120.0.0-Windows-")
            .unwrap_or_else(|| panic!("unexpected client_key `{client_key}`"));
        let (uuid, tail) = rest.split_at(36);
        let groups: Vec<usize> = uuid.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12], "{uuid}");
        assert!(
            uuid.chars()
                .filter(|c| *c != '-')
                .all(|c| c.is_ascii_hexdigit()),
            "not hexadecimal: {uuid}"
        );
        assert_eq!(&uuid[14..15], "4", "version nibble: {uuid}");
        assert!(
            matches!(&uuid[19..20], "8" | "9" | "a" | "b"),
            "variant nibble: {uuid}"
        );
        let millis = tail.strip_prefix('-').expect("a dash before the epoch");
        assert!(
            !millis.is_empty() && millis.chars().all(|c| c.is_ascii_digit()),
            "epoch milliseconds: {millis}"
        );
    }

    #[test]
    fn transmart_errors_on_a_bad_ret_code_or_a_bad_count() {
        let ok = r#"{"header":{"type":"auto_translation","ret_code":"succ","time_cost":81.0,"request_id":"x"},"auto_translation":["你好，世界。"],"src_lang":"ja","tgt_lang":"zh"}"#;
        assert_eq!(transmart_parse(ok).unwrap(), ["你好，世界。"]);
        // Trailing whitespace is trimmed off each line.
        let padded = r#"{"header":{"ret_code":"succ"},"auto_translation":["你好，世界。  "],"src_lang":"ja"}"#;
        assert_eq!(transmart_parse(padded).unwrap(), ["你好，世界。"]);

        // Anything but `succ` is the failure the service says it is.
        let bad = r#"{"header":{"ret_code":"failure","error_msg":"too many requests"}}"#;
        let error = transmart_parse(bad).unwrap_err();
        assert!(error.to_string().contains("failure"), "{error}");

        // A reply with the wrong number of lines cannot be placed again, so it
        // is refused before it is accepted.
        let error = require_count("transmart", 2, 1).unwrap_err();
        assert!(error.to_string().contains("transmart"), "{error}");
        assert!(error.to_string().contains('2'), "{error}");
        assert!(error.to_string().contains('1'), "{error}");
    }

    fn lingocloud_engine(token: &str) -> LingocloudEngine {
        LingocloudEngine {
            agent: test_agent(),
            token: token.to_owned(),
        }
    }

    #[test]
    fn lingocloud_sends_its_three_headers_and_the_spliced_pair() {
        let request = lingocloud_request(
            &lingocloud_engine("SECRET"),
            "auto",
            "zh-Hans",
            &["こんにちは、世界".to_owned(), "これはテストです".to_owned()],
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, LINGOCLOUD_ENDPOINT);
        // All three headers are required and pinned here: the charset, the
        // `token` scheme around the value, and the client's own user agent —
        // dropping any of them is a wire-format break, not a tidy-up.
        assert_eq!(
            request.headers,
            vec![
                ("Content-Type", "application/json; charset=UTF-8".to_owned()),
                ("X-Authorization", "token SECRET".to_owned()),
                ("User-Agent", LINGOCLOUD_USER_AGENT.to_owned()),
            ]
        );
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        // `auto` builds `auto2zh` with `detect:true`, and the whole batch is
        // the `source` array; `zh-Hans` is rewritten to the `zh` the endpoint
        // actually accepts.
        assert_eq!(body["trans_type"], "auto2zh");
        assert_eq!(body["detect"], true);
        assert_eq!(body["source"][0], "こんにちは、世界");
        assert_eq!(body["source"][1], "これはテストです");
        // `request_id` is the epoch in milliseconds, sent as a string.
        let request_id = body["request_id"].as_str().unwrap();
        assert!(
            !request_id.is_empty() && request_id.chars().all(|c| c.is_ascii_digit()),
            "request_id: {request_id}"
        );

        // An explicit source names both halves of the spliced pair.
        let request = lingocloud_request(&lingocloud_engine("t"), "ja", "en", &["x".to_owned()]);
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["trans_type"], "ja2en");
    }

    #[test]
    fn lingocloud_maps_only_zh_hans_and_passes_the_rest_through() {
        let engine = lingocloud_engine("t");
        let pair = |from: &str, to: &str| {
            let request = lingocloud_request(&engine, from, to, &["x".to_owned()]);
            let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
            body["trans_type"].as_str().unwrap().to_owned()
        };
        // `zh-Hans` is rejected by the endpoint on either side of the pair, so
        // it is the one tag rewritten — to `zh`.
        assert_eq!(pair("auto", "zh-Hans"), "auto2zh");
        assert_eq!(pair("zh-Hans", "en"), "zh2en");
        // `zh-Hant` is accepted and really does yield traditional Chinese, so —
        // unlike the other providers, which collapse it to simplified — it is
        // taken as written rather than thrown away.
        assert_eq!(pair("auto", "zh-Hant"), "auto2zh-Hant");
        assert_eq!(pair("zh-Hant", "ja"), "zh-Hant2ja");
        // A language the six-language web-app client would have refused still
        // goes through: the endpoint's own `rc=-1` is the error, not a drop.
        assert_eq!(pair("ja", "de"), "ja2de");
        assert_eq!(pair("ja", "ko"), "ja2ko");
    }

    #[test]
    fn lingocloud_reads_the_target_array_in_order() {
        // The captured success body: one `target` string per input line, in
        // the same order, with `trans_type` echoing the resolved pair.
        let body = r#"{"rc":0,"confidence":1,"trans_type":"ja2zh","target":["大家好，世界","这是一个测试"],"isdict":0,"dict_words":[[],[]]}"#;
        assert_eq!(
            lingocloud_parse(body).unwrap(),
            ["大家好，世界", "这是一个测试"]
        );
    }

    #[test]
    fn lingocloud_errors_on_a_bad_count_and_surfaces_the_rc_message() {
        // A reply with the wrong number of lines cannot be placed again, so it
        // is refused before it is accepted.
        let error = require_count("lingocloud", 2, 1).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("lingocloud"), "{text}");
        assert!(text.contains('2') && text.contains('1'), "{text}");

        // The captured `rc=-1` body: `message` is a real explanation, so it is
        // surfaced rather than the whole object (which also carries
        // `error_code`, `error` and `err_code`).
        let bad = r#"{"rc":-1,"error_code":491000,"error":"Unsupported trans_type","message":"Unsupported trans_type","err_code":491000}"#;
        let error = lingocloud_parse(bad).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("Unsupported trans_type"), "{text}");
        assert!(text.contains("-1"), "{text}");
        assert!(!text.contains("491000"), "the object was pasted in: {text}");

        // A success body still parses.
        assert_eq!(
            lingocloud_parse(r#"{"rc":0,"target":["甲"]}"#).unwrap(),
            ["甲"]
        );
    }

    #[test]
    fn the_configured_lingocloud_token_overrides_the_built_in() {
        use crate::config::TranslateLingocloudDefaults;
        // With no config the borrowed built-in token is used.
        match engine_from_config(&TranslateDefaults::default(), "lingocloud").unwrap() {
            Engine::Lingocloud(engine) => assert_eq!(engine.token, LINGOCLOUD_TOKEN),
            other => panic!("a bare config should resolve lingocloud, got {other:?}"),
        }

        // A configured token replaces it, and reaches the header.
        let defaults = TranslateDefaults {
            lingocloud: Some(TranslateLingocloudDefaults {
                token: Some("mine".to_owned()),
            }),
            ..Default::default()
        };
        match engine_from_config(&defaults, "lingocloud").unwrap() {
            Engine::Lingocloud(engine) => {
                assert_eq!(engine.token, "mine");
                let request = lingocloud_request(&engine, "auto", "zh", &["x".to_owned()]);
                assert!(request
                    .headers
                    .contains(&("X-Authorization", "token mine".to_owned())));
            }
            other => panic!("the override should resolve lingocloud, got {other:?}"),
        }

        // A blank token is "not set": the built-in stands.
        let blank = TranslateDefaults {
            lingocloud: Some(TranslateLingocloudDefaults {
                token: Some("  ".to_owned()),
            }),
            ..Default::default()
        };
        match engine_from_config(&blank, "lingocloud").unwrap() {
            Engine::Lingocloud(engine) => assert_eq!(engine.token, LINGOCLOUD_TOKEN),
            other => panic!("a blank token should still resolve lingocloud, got {other:?}"),
        }
    }

    #[test]
    fn bing_builds_its_authenticated_post() {
        let engine = BingEngine {
            agent: test_agent(),
            endpoint: BING_ENDPOINT.to_owned(),
            api_key: "SECRET".to_owned(),
            region: Some("westus".to_owned()),
        };
        let request = bing_request(
            &engine,
            "auto",
            "zh-Hans",
            &["a".to_owned(), "b".to_owned()],
        );
        // `auto` drops the `from` query: that is Bing's own "detect".
        assert_eq!(
            request.url,
            "https://api.cognitive.microsofttranslator.com/translate?api-version=3.0&to=zh-Hans"
        );
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.body.as_deref(),
            Some(r#"[{"Text":"a"},{"Text":"b"}]"#)
        );
        assert!(request
            .headers
            .contains(&("Ocp-Apim-Subscription-Key", "SECRET".to_owned())));
        assert!(request
            .headers
            .contains(&("Ocp-Apim-Subscription-Region", "westus".to_owned())));
        assert!(request
            .headers
            .contains(&("Content-Type", "application/json".to_owned())));

        // An explicit source appears as a `from` parameter, and a trailing
        // slash on the endpoint does not double up.
        let named = BingEngine {
            agent: test_agent(),
            endpoint: format!("{BING_ENDPOINT}/"),
            api_key: "SECRET".to_owned(),
            region: Some("westus".to_owned()),
        };
        let request = bing_request(&named, "ja", "en", &["x".to_owned()]);
        assert_eq!(
            request.url,
            "https://api.cognitive.microsofttranslator.com/translate?api-version=3.0&from=ja&to=en"
        );
        // Without a region there is no region header.
        let no_region = BingEngine {
            agent: test_agent(),
            endpoint: BING_ENDPOINT.to_owned(),
            api_key: "SECRET".to_owned(),
            region: None,
        };
        let request = bing_request(&no_region, "en", "ja", &["x".to_owned()]);
        assert!(!request
            .headers
            .iter()
            .any(|(name, _)| *name == "Ocp-Apim-Subscription-Region"));
    }

    #[test]
    fn bing_reads_its_translation_array() {
        let body = r#"[{"detectedLanguage":{"language":"en"},"translations":[{"text":"你好","to":"zh-Hans"}]},{"translations":[{"text":"世界","to":"zh-Hans"}]}]"#;
        assert_eq!(bing_parse(body).unwrap(), ["你好", "世界"]);
    }

    #[test]
    fn bing_surfaces_an_error_body() {
        let body = r#"{"error":{"code":401000,"message":"The request is not authorized because credentials are missing or invalid."}}"#;
        let error = bing_parse(body).unwrap_err();
        assert!(error.to_string().contains("not authorized"), "{error}");
    }

    fn baidu_engine() -> BaiduEngine {
        BaiduEngine {
            agent: test_agent(),
            app_id: "app".to_owned(),
            secret_key: "sec".to_owned(),
        }
    }

    #[test]
    fn baidu_signs_its_form_body() {
        let request = baidu_request(
            &baidu_engine(),
            "auto",
            "zh-Hans",
            &["hello".to_owned()],
            "salt",
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, BAIDU_ENDPOINT);
        // The signature is md5("app" + "hello" + "salt" + "sec"); the expected
        // digest is pinned so reordering the parts fails here.
        assert_eq!(
            request.body.as_deref(),
            Some(
                "q=hello&from=auto&to=zh&appid=app&salt=salt&sign=def9963d49d7df709343fab85bb31c2d"
            )
        );
        assert!(request.headers.contains(&(
            "Content-Type",
            "application/x-www-form-urlencoded".to_owned()
        )));

        // Multiple lines are joined with a newline, and the form body escapes
        // it while the signature still covers the raw text.
        let request = baidu_request(
            &baidu_engine(),
            "en",
            "ja",
            &["a".to_owned(), "b".to_owned()],
            "s",
        );
        let body = request.body.as_deref().unwrap();
        assert!(body.starts_with("q=a%0Ab&"), "{body}");
        assert_eq!(
            baidu_sign("app", "a\nb", "s", "sec"),
            // md5("app" + "a\nb" + "s" + "sec")
            body.split("sign=").nth(1).unwrap()
        );
    }

    #[test]
    fn baidu_reads_a_result_per_line() {
        let body = r#"{"from":"en","to":"zh","trans_result":[{"src":"a","dst":"甲"},{"src":"b","dst":"乙"}]}"#;
        assert_eq!(baidu_parse(body).unwrap(), ["甲", "乙"]);
    }

    #[test]
    fn baidu_reports_its_error_code() {
        let body = r#"{"error_code":"54003","error_msg":"Invalid Access Limit"}"#;
        let error = baidu_parse(body).unwrap_err();
        assert!(error.to_string().contains("54003"), "{error}");
        assert!(
            error.to_string().contains("Invalid Access Limit"),
            "{error}"
        );
        // An explicit zero, as a string or a number, is not an error.
        let ok = r#"{"error_code":"0","trans_result":[{"src":"a","dst":"甲"}]}"#;
        assert_eq!(baidu_parse(ok).unwrap(), ["甲"]);
        let numeric = r#"{"error_code":0,"trans_result":[{"src":"a","dst":"甲"}]}"#;
        assert_eq!(baidu_parse(numeric).unwrap(), ["甲"]);
    }

    fn ai_engine(endpoint: &str, prompt: &str) -> AiEngine {
        AiEngine {
            agent: test_agent(),
            endpoint: endpoint.to_owned(),
            api_key: "sk-test".to_owned(),
            model: "gpt-x".to_owned(),
            prompt: prompt.to_owned(),
        }
    }

    #[test]
    fn ai_builds_a_chat_completions_request() {
        let engine = ai_engine("https://api.example.com/v1", "");
        let request = ai_request(
            &engine,
            "auto",
            "zh-Hans",
            &["hello".to_owned(), "world".to_owned()],
        );
        assert_eq!(request.url, "https://api.example.com/v1/chat/completions");
        assert!(request
            .headers
            .contains(&("Authorization", "Bearer sk-test".to_owned())));
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("JSON array"));
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hello\nworld");

        // An endpoint that already names the path is used verbatim, and a
        // configured prompt replaces the built-in one.
        let verbatim = ai_engine(
            "https://api.example.com/v1/chat/completions",
            "Translate tersely.",
        );
        let request = ai_request(&verbatim, "en", "ja", &["x".to_owned()]);
        assert_eq!(request.url, "https://api.example.com/v1/chat/completions");
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["messages"][0]["content"], "Translate tersely.");
    }

    #[test]
    fn ai_reads_the_message_content_and_its_lines() {
        let body =
            r#"{"choices":[{"message":{"role":"assistant","content":"[\"你好\",\"世界\"]"}}]}"#;
        let content = ai_parse(body).unwrap();
        assert_eq!(ai_reply_lines(&content, 2).unwrap(), ["你好", "世界"]);
        // A fenced array is unwrapped, and the fallback reads one line per
        // line when the model ignored the JSON instruction.
        assert_eq!(
            ai_reply_lines("```json\n[\"a\",\"b\"]\n```", 2).unwrap(),
            ["a", "b"]
        );
        assert_eq!(ai_reply_lines("a\nb\n", 2).unwrap(), ["a", "b"]);
        // A count that does not match is an error naming both numbers.
        let error = ai_reply_lines("[\"a\"]", 2).unwrap_err();
        assert!(
            error.to_string().contains('1') && error.to_string().contains('2'),
            "{error}"
        );
    }

    #[test]
    fn the_external_provider_round_trips_through_a_real_child() {
        let engine = ExternalEngine {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "tr a-z A-Z".to_owned(),
            ],
            timeout: Duration::from_secs(5),
        };
        let out = engine.translate("en", "en", &["hi".to_owned(), "there".to_owned()]);
        let out: Vec<String> = out.unwrap().into_iter().map(|line| line.unwrap()).collect();
        assert_eq!(out, ["HI", "THERE"]);
    }

    #[test]
    fn an_external_provider_with_the_wrong_line_count_says_so() {
        let engine = ExternalEngine {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf 'only one\\n'".to_owned(),
            ],
            timeout: Duration::from_secs(5),
        };
        let error = engine
            .translate("en", "en", &["a".to_owned(), "b".to_owned()])
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains('1') && text.contains('2'), "{text}");
        assert!(
            text.contains("one translated line per input line"),
            "{text}"
        );
    }

    #[test]
    fn an_external_provider_that_fails_reports_its_stderr() {
        let engine = ExternalEngine {
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "echo 'no engine here' >&2; exit 4".to_owned(),
            ],
            timeout: Duration::from_secs(5),
        };
        let error = engine.translate("en", "en", &["a".to_owned()]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("no engine here"), "{text}");
        assert!(text.contains('4'), "{text}");
    }

    /// A stand-in for the network: the translations it was built with, in
    /// order, with no request made.
    struct Stub(Vec<LineResult>);

    impl Translator for Stub {
        fn translate(&self, _from: &str, _to: &str, lines: &[String]) -> Result<Vec<LineResult>> {
            assert_eq!(
                lines.len(),
                self.0.len(),
                "the stub was asked for the wrong lines"
            );
            Ok(self.0.clone())
        }
    }

    fn ocr_envelope() -> String {
        json!({
            "version": 1,
            "geometry": true,
            "lines": [
                {
                    "text": "こんにちは",
                    "rect": {"x": 0, "y": 0, "width": 10, "height": 10},
                    "chars": [{"ch": "こ", "rect": {"x": 0, "y": 0, "width": 2, "height": 10}}],
                },
                {
                    "text": "世界",
                    "rect": {"x": 0, "y": 20, "width": 10, "height": 10},
                    "chars": [],
                },
            ],
        })
        .to_string()
    }

    #[test]
    fn the_stdin_route_translates_a_canned_envelope() {
        let stub = Stub(vec![Ok("你好".to_owned()), Err("rate limited".to_owned())]);
        let output = translate_envelope(&ocr_envelope(), &stub, "google", "ja", "zh-Hans").unwrap();
        let parsed: Value = serde_json::from_str(&output.envelope).unwrap();
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["geometry"], true);
        assert_eq!(parsed["provider"], "google");
        assert_eq!(parsed["from"], "ja");
        assert_eq!(parsed["to"], "zh-Hans");
        // The first line is translated, the original kept alongside, and the
        // per-character boxes dropped.
        assert_eq!(parsed["lines"][0]["text"], "你好");
        assert_eq!(parsed["lines"][0]["source"], "こんにちは");
        assert_eq!(
            parsed["lines"][0]["rect"],
            json!({"x": 0, "y": 0, "width": 10, "height": 10})
        );
        assert!(parsed["lines"][0].get("chars").is_none());
        // A failed line keeps its source text with a short reason.
        assert_eq!(parsed["lines"][1]["text"], "世界");
        assert_eq!(parsed["lines"][1]["source"], "世界");
        assert_eq!(parsed["lines"][1]["error"], "rate limited");
        // The plain text falls back to the source for the failed line.
        assert_eq!(output.text, "你好\n世界");
    }

    #[test]
    fn the_envelope_drops_geometry_when_the_ocr_had_none() {
        let envelope = json!({
            "version": 1,
            "geometry": false,
            "lines": [{"text": "a"}, {"text": "b"}],
        })
        .to_string();
        let stub = Stub(vec![Ok("甲".to_owned()), Ok("乙".to_owned())]);
        let output = translate_envelope(&envelope, &stub, "baidu", "en", "zh-Hans").unwrap();
        let parsed: Value = serde_json::from_str(&output.envelope).unwrap();
        assert_eq!(parsed["geometry"], false);
        assert!(parsed["lines"][0].get("rect").is_none());
        assert_eq!(parsed["lines"][0]["source"], "a");
        assert_eq!(output.text, "甲\n乙");
    }

    #[test]
    fn a_bad_envelope_is_reported_rather_than_guessed_at() {
        let stub = Stub(Vec::new());
        let error = translate_envelope("not json", &stub, "google", "en", "ja").unwrap_err();
        assert!(error.to_string().contains("OCR JSON"), "{error}");
        let error = translate_envelope("{\"version\":1}", &stub, "google", "en", "ja").unwrap_err();
        assert!(error.to_string().contains("lines"), "{error}");
    }

    #[test]
    fn percent_encoding_covers_utf8_and_reserved_bytes() {
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("こん"), "%E3%81%93%E3%82%93");
        assert_eq!(percent_encode("AZ-_.~"), "AZ-_.~");
    }

    #[test]
    fn only_the_try_again_statuses_are_retryable() {
        for status in [429, 500, 502, 503, 504] {
            assert!(retryable(status), "{status} should be retried");
        }
        // A wrong key or a malformed request never succeeds on a second try.
        for status in [200, 201, 400, 401, 403, 404, 422, 501] {
            assert!(!retryable(status), "{status} should not be retried");
        }
    }

    #[test]
    fn a_transient_failure_is_retried_but_a_fatal_one_is_not() {
        // A transport error is transient: the loop tries again up to the cap
        // and returns the *last* attempt's error, naming the count.
        let attempts = std::cell::Cell::new(0);
        let error = with_retries::<()>(
            Duration::from_secs(30),
            &[Duration::from_millis(1), Duration::from_millis(1)],
            || {
                let made = attempts.get() + 1;
                attempts.set(made);
                Attempt::Transient(VshotError::Translate(format!("failure {made}")))
            },
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 3);
        assert!(error.to_string().contains("failure 3"), "{error}");
        assert!(error.to_string().contains("3 attempts"), "{error}");

        // A fatal failure is not retried at all.
        let attempts = std::cell::Cell::new(0);
        let error = with_retries::<()>(
            Duration::from_secs(30),
            &[Duration::from_millis(1), Duration::from_millis(1)],
            || {
                attempts.set(attempts.get() + 1);
                Attempt::Fatal(VshotError::Translate("401 unauthorized".into()))
            },
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(error.to_string().contains("401"), "{error}");

        // A later attempt can succeed.
        let attempts = std::cell::Cell::new(0);
        let value = with_retries(
            Duration::from_secs(30),
            &[Duration::from_millis(1), Duration::from_millis(1)],
            || {
                let made = attempts.get() + 1;
                attempts.set(made);
                if made < 3 {
                    Attempt::Transient(VshotError::Translate("dropped".into()))
                } else {
                    Attempt::Done("ok".to_owned())
                }
            },
        )
        .unwrap();
        assert_eq!(value, "ok");
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn the_deadline_cuts_the_retries_short() {
        // With no time to spare the first attempt still runs, but the 300 ms
        // pause would pass the deadline, so there is no second one.
        let attempts = std::cell::Cell::new(0);
        let error = with_retries::<()>(Duration::ZERO, &RETRY_BACKOFFS, || {
            attempts.set(attempts.get() + 1);
            Attempt::Transient(VshotError::Translate("still down".into()))
        })
        .unwrap_err();
        let text = error.to_string();
        assert_eq!(attempts.get(), 1);
        assert!(text.contains("1 attempt"), "{text}");
        assert!(!text.contains("attempts"), "{text}");
    }

    #[test]
    fn an_exhausted_retry_names_the_failure_once() {
        // The error handed in is a `Translate`, whose own `Display` already
        // reads "translation failed: ...".  Nesting one inside another printed
        // the phrase twice on every line a throttled Google failed.
        let error = with_retries::<()>(Duration::from_secs(5), &RETRY_BACKOFFS, || {
            Attempt::Transient(VshotError::Translate("google said no".into()))
        })
        .unwrap_err();
        let text = error.to_string();
        assert_eq!(text.matches("translation failed").count(), 1, "{text}");
        assert!(text.contains("google said no"), "{text}");
        assert!(text.contains("3 attempts"), "{text}");
    }

    #[test]
    fn an_html_error_page_is_described_rather_than_quoted() {
        let page = format!(
            "<!DOCTYPE html>\n<html><body>{}</body></html>",
            "x".repeat(4096)
        );
        let text = response_error("google", 429, &page).to_string();
        assert!(!text.contains("DOCTYPE"), "{text}");
        assert!(text.contains("HTML page"), "{text}");
        assert!(text.contains("429"), "{text}");
        assert!(text.chars().count() < 200, "{} chars", text.chars().count());

        // A service that explains itself in JSON still gets quoted, but only so
        // far -- and never with its newlines intact.
        let long = format!("{{\"error\":\"{}\"}}", "y".repeat(4096));
        let text = response_error("baidu", 401, &long).to_string();
        assert!(text.contains("baidu returned HTTP 401"), "{text}");
        assert!(text.chars().count() < 300, "{} chars", text.chars().count());
        assert!(!text.contains('\n'), "{text}");
    }

    #[test]
    fn the_warning_counts_the_lines_left_untranslated() {
        let stub = Stub(vec![Ok("one".to_owned()), Err("429".to_owned())]);
        let output = translate_envelope(&ocr_envelope(), &stub, "google", "ja", "en").unwrap();
        assert_eq!(output.failed_lines, 1);
        assert_eq!(output.total_lines, 2);
        assert_eq!(output.first_failure.as_deref(), Some("429"));

        let stub = Stub(vec![Ok("one".to_owned()), Ok("two".to_owned())]);
        let output = translate_envelope(&ocr_envelope(), &stub, "google", "ja", "en").unwrap();
        assert_eq!(output.failed_lines, 0);
        assert!(output.first_failure.is_none());
    }

    /// A provider that fails the whole batch, the way an HTTP provider does
    /// when the request itself fails.
    struct Failing(String);

    impl Translator for Failing {
        fn translate(&self, _from: &str, _to: &str, _lines: &[String]) -> Result<Vec<LineResult>> {
            Err(VshotError::Translate(self.0.clone()))
        }
    }

    #[test]
    fn a_non_json_google_body_is_transient_not_fatal() {
        // The "Sorry…" page Google serves a throttled client, under a 200.
        let page = "<!doctype html><html><head><title>Sorry...</title></head></html>";
        let Attempt::Transient(error) = google_body(page) else {
            panic!("an HTML page should be a transient failure");
        };
        let text = error.to_string();
        assert!(text.contains("rate-limit"), "{text}");
        assert!(text.contains("JSON"), "{text}");

        // A JSON body of the wrong shape is the service's answer: not retried.
        assert!(matches!(google_body("[1,2,3]"), Attempt::Fatal(_)));

        // A real `dj=1` body still parses.
        let body = r#"{"sentences":[{"trans":"你好","orig":"hello","backend":10}],"src":"en"}"#;
        assert!(matches!(google_body(body), Attempt::Done(text) if text == "你好"));
    }

    #[test]
    fn auto_skips_providers_without_their_credentials() {
        use crate::config::{TranslateAiDefaults, TranslateBingDefaults};
        // The five credential-free-by-config providers always run and come
        // first — `lingocloud` last of them, on its borrowed token; `bing` has
        // a key and is usable; `ai` names an endpoint but no model, and `baidu`
        // and `external` have nothing at all, so the usable order is the five
        // keyless ones followed by bing.
        let defaults = TranslateDefaults {
            bing: Some(TranslateBingDefaults {
                api_key: Some("k".to_owned()),
                ..Default::default()
            }),
            ai: Some(TranslateAiDefaults {
                endpoint: Some("https://example.com/v1".to_owned()),
                api_key: Some("k".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let names: Vec<String> = engine_chain(&defaults, AUTO)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "google",
                "microsoft",
                "volcengine",
                "transmart",
                "lingocloud",
                "bing"
            ]
        );

        // The keyless providers are first in the built-in order: the ones a
        // user can get an answer from without configuring anything — and
        // `lingocloud` is right after them, before the keyed ones, because it
        // runs on a credential that is not vshot's.
        let keyless = [
            Kind::Google,
            Kind::Microsoft,
            Kind::Volcengine,
            Kind::Transmart,
            Kind::Lingocloud,
        ];
        assert_eq!(&AUTO_ORDER[..keyless.len()], &keyless);
        assert_eq!(AUTO_ORDER[keyless.len()], Kind::Bing);
    }

    #[test]
    fn auto_with_no_usable_provider_names_every_missing_piece() {
        // `google` is always usable, so the built-in order never comes up
        // empty; the same resolver over only the credentialled providers shows
        // the error the order would give if none of them were usable.
        let error = pick_usable(
            &TranslateDefaults::default(),
            &[Kind::Bing, Kind::Baidu, Kind::Ai, Kind::External],
        )
        .unwrap_err();
        let text = error.to_string();
        for needle in ["api-key", "app-id", "endpoint", "external.command"] {
            assert!(text.contains(needle), "{needle} missing from `{text}`");
        }
    }

    #[test]
    fn a_provider_that_fails_every_line_falls_through_to_the_next() {
        let down = Stub(vec![
            Err("throttled".to_owned()),
            Err("throttled".to_owned()),
        ]);
        let up = Stub(vec![Ok("甲".to_owned()), Ok("乙".to_owned())]);

        // Every line failed, so the chain moves on.
        let chain: [(&str, &dyn Translator); 2] = [("first", &down), ("second", &up)];
        let (name, results) =
            translate_chain(&chain, "ja", "zh-Hans", &["a".to_owned(), "b".to_owned()]).unwrap();
        assert_eq!(name, "second");
        assert!(results.iter().all(LineResult::is_ok));

        // A single success is enough: the first provider is kept as-is, its
        // other per-line failures included.
        let partial = Stub(vec![Ok("甲".to_owned()), Err("throttled".to_owned())]);
        let chain: [(&str, &dyn Translator); 2] = [("first", &partial), ("second", &up)];
        let (name, results) =
            translate_chain(&chain, "ja", "zh-Hans", &["a".to_owned(), "b".to_owned()]).unwrap();
        assert_eq!(name, "first");
        assert_eq!(results[0], Ok("甲".to_owned()));
        assert_eq!(results[1], Err("throttled".to_owned()));
    }

    #[test]
    fn an_exhausted_chain_names_every_provider_it_tried() {
        let per_line = Stub(vec![Err("the last line said no".to_owned())]);
        let batch = Failing("the service is down".to_owned());
        let chain: [(&str, &dyn Translator); 2] = [("first", &per_line), ("second", &batch)];
        let error = translate_chain(&chain, "ja", "zh-Hans", &["a".to_owned()]).unwrap_err();
        let text = error.to_string();
        for needle in [
            "first",
            "the last line said no",
            "second",
            "the service is down",
        ] {
            assert!(text.contains(needle), "{needle} missing from `{text}`");
        }
    }

    #[test]
    fn fallback_is_deduplicated_against_the_primary() {
        use crate::config::TranslateBingDefaults;
        let defaults = TranslateDefaults {
            bing: Some(TranslateBingDefaults {
                api_key: Some("k".to_owned()),
                ..Default::default()
            }),
            fallback: Some(vec![
                "google".to_owned(),
                "bing".to_owned(),
                "bing".to_owned(),
            ]),
            ..Default::default()
        };
        // A `google` primary followed by `["google", "bing", "bing"]` is just
        // google then bing, each run once.
        let names: Vec<String> = engine_chain(&defaults, "google")
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["google", "bing"]);

        // The same names under `auto` add nothing: auto already lists google
        // and bing, and the keyless providers in between are new.
        let names: Vec<String> = engine_chain(&defaults, AUTO)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "google",
                "microsoft",
                "volcengine",
                "transmart",
                "lingocloud",
                "bing"
            ]
        );
    }

    #[test]
    fn an_unknown_fallback_name_is_refused() {
        let defaults = TranslateDefaults {
            fallback: Some(vec!["deepl".to_owned()]),
            ..Default::default()
        };
        let error = engine_chain(&defaults, "google").unwrap_err();
        assert!(error.to_string().contains("deepl"), "{error}");
    }

    #[test]
    fn the_envelope_names_the_provider_that_answered() {
        let down = Stub(vec![
            Err("throttled".to_owned()),
            Err("throttled".to_owned()),
        ]);
        let up = Stub(vec![Ok("你好".to_owned()), Ok("世界".to_owned())]);
        let chain: [(&str, &dyn Translator); 2] = [("google", &down), ("bing", &up)];
        let output = translate_envelope_chain(&ocr_envelope(), &chain, "ja", "zh-Hans").unwrap();
        let parsed: Value = serde_json::from_str(&output.envelope).unwrap();
        assert_eq!(parsed["provider"], "bing");
    }

    #[test]
    fn a_lone_provider_keeps_an_all_failed_batch_as_an_envelope() {
        // No `fallback` means the chain is one provider long, and its result is
        // the answer even when every line failed: the Qt overlay reads the
        // per-line `error` fields, so turning that into a batch error would be
        // a behaviour change the empty default promises not to make.
        let stub = Stub(vec![
            Err("throttled".to_owned()),
            Err("throttled".to_owned()),
        ]);
        let output = translate_envelope(&ocr_envelope(), &stub, "google", "ja", "en").unwrap();
        assert_eq!(output.failed_lines, 2);
        assert_eq!(output.first_failure.as_deref(), Some("throttled"));
    }
}
