// ── Live fetch ───────────────────────────────────────────────────────────
//
// Fetch concurrency permits, agent cache, hydrator and fetchers.
//
// Split from `live.rs`; re-exported there, `crate::live::…` paths unchanged.

use crate::caps::{MAX_LIVE_ITEMS, MAX_LIVE_RESPONSE_BYTES};
use crate::live_cache::{ResourceKind, sanitize_resource_values};
use crate::live_config::{CustomResource, LiveConfig};
use crate::live_net::{
    LiveError, build_custom_rest_url, build_rest_url, read_ca_bundle, resolve_and_validate_host,
    spki_sha256,
};
use crate::logging::{log_debug, log_info, log_warn, redact_secrets, sanitize_for_log};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

// ── Bounded fetch concurrency (A-03) ─────────────────────────────────────
//
// `trigger_background_fetch` historically spawned one thread per cache
// miss, coalesced only by the 2 s window (`LIVE_FETCH_BLOCKING_TIMEOUT_SECS`).
// Under flapping completions that still allowed bursts. Cap concurrency
// to 2 threads globally via an `AtomicUsize` semaphore — smallest safe
// change, no new dependency, preserves the existing coalescing window.
static ACTIVE_FETCHES: AtomicUsize = AtomicUsize::new(0);
pub(crate) const MAX_CONCURRENT_FETCHES: usize = 2;

pub(crate) fn try_acquire_fetch_permit() -> bool {
    let mut cur = ACTIVE_FETCHES.load(Ordering::Acquire);
    loop {
        if cur >= MAX_CONCURRENT_FETCHES {
            return false;
        }
        match ACTIVE_FETCHES.compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(next) => cur = next,
        }
    }
}

fn release_fetch_permit() {
    ACTIVE_FETCHES.fetch_sub(1, Ordering::AcqRel);
}

pub(crate) struct FetchPermitGuard;
impl Drop for FetchPermitGuard {
    fn drop(&mut self) {
        release_fetch_permit();
    }
}

// ── Fetch ────────────────────────────────────────────────────────────────

/// Get a cached `ureq::Agent` for the given timeout and TLS verification mode, or build a new one.
///
/// Uses a global `OnceLock` cache keyed by `(timeout_secs, ssl_verify)` to reuse agents across
/// calls.
/// Logs `live agent reuse` on hit. Prefer `get_cached_agent_for_config` (pin/CA aware); this
/// wrapper exists for unit tests and pin-less call sites.
pub(crate) fn get_cached_agent(timeout: Duration, ssl_verify: bool) -> ureq::Agent {
    static AGENT_CACHE: OnceLock<Mutex<HashMap<(u64, bool), ureq::Agent>>> = OnceLock::new();
    let cache = AGENT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (timeout.as_secs(), ssl_verify);
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        if let Some(agent) = guard.get(&key) {
            log_debug!(
                "live agent reuse timeout={}s ssl_verify={} ssl_verify_effective={}",
                key.0,
                key.1,
                key.1
            );
            return agent.clone();
        }
    }
    // Build new agent
    // Redirects are disabled (`.redirects(0)`): the code treats 3xx as
    // `LiveError::Status`, so nothing is lost and open-redirect SSRF is closed.
    let agent = if ssl_verify {
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build()
    } else {
        log_warn!(
            "live ssl_verify=false — building agent with insecure TLS verifier (host verification disabled)"
        );
        match build_insecure_agent(timeout) {
            Some(a) => a,
            None => {
                log_warn!(
                    "live insecure agent build failed, falling back to default verifier (verification will still be attempted)"
                );
                ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build()
            }
        }
    };
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        guard.insert(key, agent.clone());
    }
    agent
}

/// Pin-aware agent cache key: timeout + effective verify + pin + CA path.
fn agent_cache_key_for_config(
    config: &LiveConfig,
    timeout: Duration,
) -> (u64, bool, Option<[u8; 32]>, String) {
    (
        timeout.as_secs(),
        config.ssl_verify_effective(),
        config.fingerprint,
        config.ca_file.clone(),
    )
}

/// Get (or build) the `ureq::Agent` for a full `LiveConfig`.
///
/// Selection order on `https`:
///
/// 1. Pin set (+ optional custom CA roots) => SPKI-pinning verifier.
/// 2. Custom CA file only => chain verifier against those roots.
/// 3. `ssl_verify_effective()` true => default platform roots.
/// 4. Otherwise => insecure verifier + WARN (preserves `MIKROTIK_SSL=0`).
///
/// On `http` no TLS is involved; a plain agent is returned. Redirects are
/// always disabled (3xx surfaces as `LiveError::Status`).
pub(crate) fn get_cached_agent_for_config(config: &LiveConfig, timeout: Duration) -> ureq::Agent {
    type PinnedAgentCacheKey = (u64, bool, Option<[u8; 32]>, String);
    static PINNED_CACHE: OnceLock<Mutex<HashMap<PinnedAgentCacheKey, ureq::Agent>>> =
        OnceLock::new();
    let cache = PINNED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = agent_cache_key_for_config(config, timeout);
    {
        let guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        if let Some(agent) = guard.get(&key) {
            log_debug!(
                "live agent reuse timeout={}s ssl_verify_effective={} pin_set={} ca_set={}",
                key.0,
                key.1,
                key.2.is_some(),
                !key.3.is_empty()
            );
            return agent.clone();
        }
    }
    let agent = build_agent_for_config(config, timeout);
    {
        let mut guard = cache.lock().unwrap_or_else(|e| {
            log_warn!("agent cache lock poisoned, recovering");
            e.into_inner()
        });
        guard.insert(key, agent.clone());
    }
    agent
}

/// Build (uncached) the agent for a config; fail-closed fallbacks never
/// silently downgrade to insecure: on pin/CA build failure a default
/// verifying agent is returned so the handshake still validates.
fn build_agent_for_config(config: &LiveConfig, timeout: Duration) -> ureq::Agent {
    if config.scheme() != "https" {
        return ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build();
    }
    if let Some(pin) = config.fingerprint {
        if !config.ca_file.is_empty() {
            match build_pinned_with_ca_agent(timeout, pin, &config.ca_file) {
                Some(a) => {
                    log_info!("live tls pin + custom CA active");
                    return a;
                }
                None => {
                    log_warn!(
                        "live pin/CA agent build failed, falling back to default verifier (fail-closed, verification still attempted)"
                    );
                    return ureq::AgentBuilder::new()
                        .timeout(timeout)
                        .redirects(0)
                        .build();
                }
            }
        }
        match build_pinned_agent(timeout, pin) {
            Some(a) => {
                log_info!("live tls SPKI pin active (chain replaced by pin check)");
                return a;
            }
            None => {
                log_warn!(
                    "live pinned agent build failed, falling back to default verifier (fail-closed)"
                );
                return ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build();
            }
        }
    }
    if !config.ca_file.is_empty() {
        match build_ca_agent(timeout, &config.ca_file) {
            Some(a) => {
                log_info!("live custom CA active");
                return a;
            }
            None => {
                log_warn!(
                    "live custom CA agent build failed, falling back to default verifier (fail-closed)"
                );
                return ureq::AgentBuilder::new()
                    .timeout(timeout)
                    .redirects(0)
                    .build();
            }
        }
    }
    // No pin/CA: reuse the legacy path (secure default or insecure + WARN).
    get_cached_agent(timeout, config.ssl_verify_effective())
}

/// SPKI-pinning verifier: accepts only a leaf whose SPKI SHA256 equals `pin`.
///
/// Chain validation is replaced by the pin check (TOFU-style pinning, no new
/// dependency on a roots bundle). Any mismatch or malformed cert fails the
/// handshake fail-closed. The TLS `CertificateVerify`/`ServerKeyExchange`
/// signature is still verified against the presented leaf's public key using
/// ring's signature algorithms, so an on-path attacker who replays the public
/// pinned certificate without its private key cannot complete the handshake.
#[derive(Debug)]
struct SpkiPinVerifier {
    pin: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match spki_sha256(end_entity.as_ref()) {
            Some(digest) if digest == self.pin => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::ring::default_provider();
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::ring::default_provider();
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build an agent that pins the leaf SPKI instead of chain validation.
fn build_pinned_agent(timeout: Duration, pin: [u8; 32]) -> Option<ureq::Agent> {
    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SpkiPinVerifier { pin }))
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Test-only entry point to the pinned verifier's handshake-signature checks.
///
/// The pin value is irrelevant to signature verification, so a zero pin is
/// used. Returns the TLS 1.2 and TLS 1.3 results so tests can assert that a
/// bogus signature fails closed instead of being asserted.
#[cfg(test)]
pub(crate) fn verify_pinned_handshake_signatures_for_test(
    cert: &rustls::pki_types::CertificateDer<'_>,
    dss: &rustls::DigitallySignedStruct,
) -> (
    Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>,
    Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>,
) {
    use rustls::client::danger::ServerCertVerifier;
    let verifier = SpkiPinVerifier { pin: [0u8; 32] };
    let message = b"live-tls-test-message";
    (
        verifier.verify_tls12_signature(message, cert, dss),
        verifier.verify_tls13_signature(message, cert, dss),
    )
}

/// Parse PEM `CERTIFICATE` blocks from `pem_text` into DER certificates.
///
/// Minimal parser using the existing `base64` dependency (no new crates):
/// splits on BEGIN/END markers and base64-decodes each block. Non-certificate
/// blocks are skipped; returns the successfully decoded certs.
pub(crate) fn parse_pem_certs(pem_text: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    use base64::Engine;
    let mut out = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem_text.lines() {
        let t = line.trim();
        if t == "-----BEGIN CERTIFICATE-----" {
            in_block = true;
            b64.clear();
            continue;
        }
        if t == "-----END CERTIFICATE-----" {
            if in_block {
                let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(&compact) {
                    out.push(rustls::pki_types::CertificateDer::from(der));
                } else {
                    log_warn!("live CA file: skipping undecodable PEM block");
                }
            }
            in_block = false;
            b64.clear();
            continue;
        }
        if in_block {
            b64.push_str(t);
        }
    }
    out
}

/// Build an agent validating chains against a user-provided PEM CA bundle.
///
/// Fail-closed `None` when the file is missing, unreadable, oversize
/// (>256 KiB), or contains no decodable certificates. The path itself is
/// sanitized in logs. Repeated failures for the same canonical path are
/// served from a negative cache (no re-read per keystroke).
fn build_ca_agent(timeout: Duration, ca_file: &str) -> Option<ureq::Agent> {
    let text = read_ca_bundle(ca_file)?;
    let certs = parse_pem_certs(&text);
    if certs.is_empty() {
        log_warn!(
            "live CA file has no decodable certificates: {:?}",
            sanitize_for_log(ca_file)
        );
        return None;
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        if roots.add(cert).is_err() {
            log_warn!(
                "live CA file: skipping invalid certificate for {:?}",
                sanitize_for_log(ca_file)
            );
        }
    }
    if roots.is_empty() {
        return None;
    }
    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Combined verifier: chain must validate against the custom CA bundle AND
/// the leaf SPKI pin must match. Both checks fail-closed.
#[derive(Debug)]
struct PinnedWithCaVerifier {
    pin: [u8; 32],
    inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedWithCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        match spki_sha256(end_entity.as_ref()) {
            Some(digest) if digest == self.pin => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build an agent combining custom CA chain validation with an SPKI pin.
fn build_pinned_with_ca_agent(
    timeout: Duration,
    pin: [u8; 32],
    ca_file: &str,
) -> Option<ureq::Agent> {
    let text = read_ca_bundle(ca_file)?;
    let certs = parse_pem_certs(&text);
    if certs.is_empty() {
        log_warn!(
            "live pin+CA file has no decodable certificates: {:?}",
            sanitize_for_log(ca_file)
        );
        return None;
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return None;
    }
    let provider = rustls::crypto::ring::default_provider();
    let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .ok()?;
    let verifier = PinnedWithCaVerifier { pin, inner };
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Some(
        ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .tls_config(Arc::new(tls_config))
            .build(),
    )
}

/// Build an agent that disables TLS verification (insecure).
///
/// Returns `None` if the rustls insecure config cannot be built.
pub(crate) fn build_insecure_agent(timeout: Duration) -> Option<ureq::Agent> {
    // Use rustls dangerous verifier that accepts any certificate.
    use rustls::DigitallySignedStruct;
    use rustls::SignatureScheme;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    #[derive(Debug)]
    struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            // Use ring's supported schemes; provider is available via rustls crypto.
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
        .tls_config(Arc::new(tls_config))
        .build();
    Some(agent)
}

/// Shared live-fetch path used by both `fetch_resource` and
/// `fetch_custom_resource`.
///
/// Performs the authenticated GET against `url` with the config's clamped
/// timeout, enforces the response caps from `caps.rs`
/// (`MAX_LIVE_RESPONSE_BYTES` via `reader.take(limit + 1)`), validates the
/// JSON-array shape, and extracts + sanitizes the values via
/// `extract_and_sanitize`.
///
/// `label` identifies the resource in logs (e.g. `Interfaces` or a custom
/// property name). Callers are responsible for `config.is_active()`, host
/// validation, and URL construction (via `build_base_url_with_allow`). `pass` is only
/// used in the Authorization header and never logged.
fn fetch_live_resource(
    config: &LiveConfig,
    url: &str,
    json_field: &str,
    kind: ResourceKind,
    label: &str,
) -> Result<Vec<String>, LiveError> {
    if !config.ssl_verify && config.fingerprint.is_none() && config.ca_file.is_empty() {
        log_warn!(
            "live ssl_verify=false — TLS verification disabled (insecure) scheme={} host={} port={} ssl_verify_effective={}",
            config.scheme(),
            sanitize_for_log(&config.host),
            config.port,
            config.ssl_verify_effective()
        );
    }
    if config.fingerprint_invalid {
        return Err(LiveError::Network(
            "invalid MIKROTIK_FINGERPRINT".to_string(),
        ));
    }
    // F1: resolve-then-revalidate at fetch time. Lexical checks ran at
    // config time; DNS may resolve differently now. Any denied resolved IP
    // (or resolution failure) fails closed before credentials are sent.
    // IP literals resolve locally without DNS traffic.
    resolve_and_validate_host(&config.host, config.port, config.allow_loopback)?;
    let timeout = Duration::from_secs(config.timeout_secs.clamp(1, 30));
    let agent = get_cached_agent_for_config(config, timeout);

    let start = Instant::now();
    let credentials = format!("{}:{}", config.user, config.pass);
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        credentials.as_bytes(),
    );
    let auth_header = format!("Basic {encoded}");

    let resp: Result<ureq::Response, ureq::Error> = agent
        .get(url)
        .set("Accept", "application/json")
        .set("Authorization", &auth_header)
        .call();

    let response: ureq::Response = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(LiveError::Status(code)),
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            if msg.to_ascii_lowercase().contains("timed out")
                || msg.to_ascii_lowercase().contains("timeout")
            {
                return Err(LiveError::Timeout);
            }
            // F6: transport errors echo URLs/headers on some stacks — strip
            // password and Basic material before storing.
            return Err(LiveError::Network(redact_secrets(
                &msg,
                &config.pass,
                &config.user,
            )));
        }
    };

    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(LiveError::Status(status));
    }

    let reader = response.into_reader();
    let mut buf = Vec::new();
    let limit = MAX_LIVE_RESPONSE_BYTES + 1;
    let n = {
        use std::io::Read;
        let mut limited = reader.take(limit as u64);
        match limited.read_to_end(&mut buf) {
            Ok(n) => n,
            // F6: I/O errors may echo request context — redact before storing.
            Err(e) => {
                return Err(LiveError::Network(redact_secrets(
                    &e.to_string(),
                    &config.pass,
                    &config.user,
                )));
            }
        }
    };
    if n > MAX_LIVE_RESPONSE_BYTES {
        return Err(LiveError::ResponseTooLarge(n));
    }
    if buf.is_empty() {
        return Err(LiveError::Parse("empty response".to_string()));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|e| LiveError::Parse(format!("invalid json: {e}")))?;
    let Some(arr) = json.as_array() else {
        return Err(LiveError::Parse("expected JSON array".to_string()));
    };

    let cleaned = extract_and_sanitize(arr, json_field, kind);
    if cleaned.is_empty() && !arr.is_empty() {
        log_warn!(
            "live fetch parsed 0 valid values for {label} from {} entries",
            arr.len()
        );
    }
    let elapsed = start.elapsed();
    log_debug!(
        "live fetch completed {label} host={} latency_ms={} items={} elapsed={:?}",
        sanitize_for_log(&config.host),
        elapsed.as_millis(),
        cleaned.len(),
        elapsed
    );
    Ok(cleaned)
}

/// Extract `json_field` from each array entry (bounded to
/// `2 * MAX_LIVE_ITEMS` raw values) and sanitize the results with `kind`'s
/// value filter.
pub(crate) fn extract_and_sanitize(
    arr: &[serde_json::Value],
    json_field: &str,
    kind: ResourceKind,
) -> Vec<String> {
    let mut raw_values: Vec<String> = Vec::new();
    for entry in arr {
        if let Some(obj) = entry.as_object()
            && let Some(val) = obj.get(json_field)
            && let Some(val_str) = val.as_str()
        {
            raw_values.push(val_str.to_string());
        }
        if raw_values.len() >= MAX_LIVE_ITEMS * 2 {
            break;
        }
    }
    sanitize_resource_values(raw_values, kind)
}

/// Fetch live data for a specific resource kind from the RouterOS REST API.
///
/// Thin wrapper over `fetch_live_resource`: builds the resource-specific URL
/// (which runs the shared host/port/scheme validation via `build_base_url_with_allow`)
/// and selects the resource's JSON field and value filter.
pub fn fetch_resource(
    config: &LiveConfig,
    resource: ResourceKind,
) -> Result<Vec<String>, LiveError> {
    if !config.is_active() {
        return Err(LiveError::Disabled);
    }
    let url = build_rest_url(config, resource)?;
    log_debug!(
        "live fetch_resource kind={:?} url={} user={} timeout={}s ssl_verify={} ssl_verify_effective={}",
        resource,
        sanitize_for_log(&url),
        sanitize_for_log(&config.user),
        config.timeout_secs,
        config.ssl_verify,
        config.ssl_verify_effective()
    );
    fetch_live_resource(
        config,
        &url,
        resource.json_field(),
        resource,
        &format!("{resource:?}"),
    )
}

/// Fetch live data for a custom resource (user-defined via
/// `RSC_LS_LIVE_RESOURCES`).
///
/// Thin wrapper over `fetch_live_resource`; custom values use the generic
/// identifier filter (same as interfaces).
pub fn fetch_custom_resource(
    config: &LiveConfig,
    custom: &CustomResource,
) -> Result<Vec<String>, LiveError> {
    if !config.is_active() {
        return Err(LiveError::Disabled);
    }
    let url = build_custom_rest_url(config, custom)?;
    log_debug!(
        "live fetch_custom kind={} url={} user={} timeout={}s",
        sanitize_for_log(&custom.property),
        sanitize_for_log(&url),
        sanitize_for_log(&config.user),
        config.timeout_secs
    );
    fetch_live_resource(
        config,
        &url,
        &custom.field,
        ResourceKind::Interfaces,
        &custom.property,
    )
}

/// Fetch interface names from the RouterOS REST API (wrapper for backwards compatibility).
#[cfg(test)]
pub fn fetch_interfaces(config: &LiveConfig) -> Result<Vec<String>, LiveError> {
    fetch_resource(config, ResourceKind::Interfaces)
}
