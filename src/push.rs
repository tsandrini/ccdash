//! Web Push (RFC 8030) from the hub, next to ntfy. The panel installed on
//! a phone hands its subscription to `/api/v1/push/subscriptions`; every
//! notification is then encrypted for that device (RFC 8291, `aes128gcm`),
//! signed with the hub's VAPID key (RFC 8292) and posted to the push
//! service. The service worker shows it with buttons that answer through
//! `/act/{key}/{action}`; a `close` message takes it down when the prompt
//! ends. Subscriptions the push service reports gone (404, 410) are
//! dropped.

use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    aead, agreement, digest, hkdf,
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    config,
    notify::{Note, Sink},
    store::Store,
    util::{now_ms, truncate},
};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS push_subscriptions (
    id INTEGER PRIMARY KEY,
    endpoint TEXT NOT NULL UNIQUE,
    p256dh TEXT NOT NULL,
    auth TEXT NOT NULL,
    user_agent TEXT,
    created_at INTEGER NOT NULL,
    last_success_at INTEGER,
    last_failure_at INTEGER,
    last_failure TEXT
);
";

/// One record of the default size: 4096 bytes less the tag and the
/// delimiter; push services take about that much per message.
const MAX_PLAINTEXT: usize = 4079;
const RECORD_SIZE: u32 = 4096;
const JWT_LIFETIME_SECS: i64 = 12 * 3600;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The hub's VAPID key pair.
pub struct Keys {
    pair: EcdsaKeyPair,
    rng: SystemRandom,
}

impl Keys {
    /// Loads the PKCS#8 key pair at `path`, generating one readable by the
    /// owner only when there is none yet.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let rng = SystemRandom::new();
        let der = match std::fs::read(path) {
            Ok(der) => der,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow!("generating the VAPID key pair"))?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .with_context(|| format!("creating {}", path.display()))?
                    .write_all(doc.as_ref())?;
                info!("generated the VAPID key pair at {}", path.display());
                doc.as_ref().to_vec()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng)
            .map_err(|e| anyhow!("{}: {e}", path.display()))?;
        Ok(Self { pair, rng })
    }

    /// The public key as the panel's `applicationServerKey`: the
    /// uncompressed P-256 point, base64url.
    pub fn public_key(&self) -> String {
        B64.encode(self.pair.public_key().as_ref())
    }

    /// The `Authorization` header for the push service behind `endpoint`:
    /// an ES256 JWT for its origin plus the public key (RFC 8292).
    fn authorization(&self, endpoint: &str, subject: &str, now_secs: i64) -> Result<String> {
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = json!({
            "aud": origin(endpoint)?,
            "exp": now_secs + JWT_LIFETIME_SECS,
            "sub": subject,
        });
        let input = format!("{header}.{}", B64.encode(claims.to_string()));
        let signature = self
            .pair
            .sign(&self.rng, input.as_bytes())
            .map_err(|_| anyhow!("signing the VAPID token"))?;
        Ok(format!(
            "vapid t={input}.{}, k={}",
            B64.encode(signature.as_ref()),
            self.public_key()
        ))
    }
}

/// `scheme://host[:port]` of a push endpoint: `https`, or plain `http` on
/// this machine (a fake push service while testing).
fn origin(url: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .context("the endpoint has no scheme")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let loopback =
        host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]");
    if host.is_empty() || !(scheme == "https" || (scheme == "http" && loopback)) {
        bail!("the endpoint is not an https URL");
    }
    Ok(format!("{scheme}://{host}"))
}

/// Encrypts `plaintext` for the subscription's `p256dh` key and `auth`
/// secret (RFC 8291): one `aes128gcm` record, no padding.
pub fn encrypt(
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
    rng: &SystemRandom,
) -> Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)?;
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng)?;
    let as_public = private.compute_public_key()?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let shared = agreement::agree_ephemeral(private, &peer, |s| s.to_vec())
        .map_err(|_| anyhow!("the subscription's key is not a P-256 point"))?;
    encrypt_with(
        &shared,
        &salt,
        as_public.as_ref(),
        ua_public,
        auth,
        plaintext,
    )
}

/// The key schedule and the record, given the ECDH result (the testable
/// half of [`encrypt`]).
fn encrypt_with(
    shared: &[u8],
    salt: &[u8; 16],
    as_public: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT {
        bail!("payload too large ({} bytes)", plaintext.len());
    }
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = expand(
        &hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(shared),
        &key_info,
        32,
    )?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = expand(&prk, b"Content-Encoding: nonce\0", 12)?;
    let mut record = plaintext.to_vec();
    // NOTE: 0x02 marks the last record (RFC 8188 §2)
    record.push(2);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek)?);
    key.seal_in_place_append_tag(
        aead::Nonce::try_assume_unique_for_key(&nonce)?,
        aead::Aad::empty(),
        &mut record,
    )?;
    let mut out = Vec::with_capacity(21 + as_public.len() + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(u8::try_from(as_public.len())?);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len))?.fill(&mut out)?;
    Ok(out)
}

/// Decodes base64 as browsers hand it out: URL-safe or standard alphabet,
/// padded or not.
fn decode(text: &str) -> Result<Vec<u8>> {
    let text: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    Ok(B64.decode(text)?)
}

/// `Topic` header: the push service replaces an undelivered message by a
/// later one with the same topic (32 URL-safe characters at most).
fn topic(seq: &str) -> String {
    let hash = digest::digest(&digest::SHA256, seq.as_bytes());
    B64.encode(hash.as_ref()).chars().take(32).collect()
}

// --- subscriptions ----------------------------------------------------------

/// What the browser's `PushSubscription.toJSON()` gives.
#[derive(Debug, Clone, Deserialize)]
pub struct Subscribe {
    pub endpoint: String,
    pub keys: SubscribeKeys,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeKeys {
    pub p256dh: String,
    pub auth: String,
}

impl Subscribe {
    /// Checks the endpoint and the key shapes before storing anything.
    pub fn validate(&self) -> Result<()> {
        origin(&self.endpoint)?;
        if self.endpoint.len() > 2048 {
            bail!("the endpoint is too long");
        }
        let p256dh = decode(&self.keys.p256dh).context("p256dh")?;
        if p256dh.len() != 65 || p256dh[0] != 4 {
            bail!("p256dh is not an uncompressed P-256 point");
        }
        if decode(&self.keys.auth).context("auth")?.len() != 16 {
            bail!("auth is not 16 bytes");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    pub id: i64,
    pub endpoint: String,
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub last_success_at: Option<i64>,
    pub last_failure_at: Option<i64>,
    pub last_failure: Option<String>,
}

const COLUMNS: &str = "id, endpoint, p256dh, auth, user_agent, created_at, \
    last_success_at, last_failure_at, last_failure";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: r.get(0)?,
        endpoint: r.get(1)?,
        p256dh: r.get(2)?,
        auth: r.get(3)?,
        user_agent: r.get(4)?,
        created_at: r.get(5)?,
        last_success_at: r.get(6)?,
        last_failure_at: r.get(7)?,
        last_failure: r.get(8)?,
    })
}

pub fn subscriptions(conn: &Connection) -> Result<Vec<Subscription>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM push_subscriptions ORDER BY id"
    ))?;
    let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Adds the subscription, or refreshes the keys of a known endpoint.
pub fn subscribe(
    conn: &Connection,
    sub: &Subscribe,
    user_agent: Option<&str>,
    now: i64,
) -> Result<Subscription> {
    conn.execute(
        "INSERT INTO push_subscriptions (endpoint, p256dh, auth, user_agent, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT (endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth,
                user_agent = excluded.user_agent, last_failure_at = NULL, last_failure = NULL",
        params![
            sub.endpoint,
            sub.keys.p256dh,
            sub.keys.auth,
            user_agent,
            now
        ],
    )?;
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM push_subscriptions WHERE endpoint = ?1"),
        [&sub.endpoint],
        row,
    )
    .optional()?
    .context("subscription vanished")
}

/// Removes the subscription; `false` when there was none.
pub fn unsubscribe(conn: &Connection, endpoint: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM push_subscriptions WHERE endpoint = ?1",
        [endpoint],
    )? > 0)
}

fn settle(conn: &Connection, id: i64, outcome: &Outcome, now: i64) -> Result<()> {
    match outcome {
        Outcome::Sent => conn.execute(
            "UPDATE push_subscriptions SET last_success_at = ?2, last_failure_at = NULL,
                last_failure = NULL WHERE id = ?1",
            params![id, now],
        )?,
        Outcome::Dropped { .. } => {
            conn.execute("DELETE FROM push_subscriptions WHERE id = ?1", [id])?
        }
        Outcome::Failed { error } => conn.execute(
            "UPDATE push_subscriptions SET last_failure_at = ?2, last_failure = ?3 WHERE id = ?1",
            params![id, now, error],
        )?,
    };
    Ok(())
}

// --- sending ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Sent,
    /// The push service no longer knows the subscription; it was deleted.
    Dropped {
        status: u16,
    },
    Failed {
        error: String,
    },
}

/// Sends messages to every stored subscription.
pub struct Push {
    keys: Keys,
    store: Store,
    subject: String,
    ttl_secs: u64,
}

impl Push {
    pub fn new(keys: Keys, store: Store, cfg: &config::Push, public_url: &str) -> Arc<Self> {
        Arc::new(Self {
            keys,
            store,
            subject: cfg.subject.clone().unwrap_or_else(|| public_url.to_owned()),
            ttl_secs: cfg.ttl_secs,
        })
    }

    pub fn public_key(&self) -> String {
        self.keys.public_key()
    }

    /// Sends `payload` to every subscription and records how it went; a
    /// later message with the same `topic` replaces an undelivered one.
    pub async fn send_all(
        self: &Arc<Self>,
        payload: &Value,
        urgent: bool,
        topic: &str,
    ) -> Result<Vec<(Subscription, Outcome)>> {
        let store = self.store.clone();
        let subs = tokio::task::spawn_blocking(move || subscriptions(&store.reader()?)).await??;
        let body = payload.to_string();
        let mut out = Vec::with_capacity(subs.len());
        for sub in subs {
            let (this, s, body, topic) =
                (self.clone(), sub.clone(), body.clone(), topic.to_owned());
            let outcome =
                tokio::task::spawn_blocking(move || this.send(&s, body.as_bytes(), urgent, &topic))
                    .await?;
            if let Outcome::Dropped { status } = &outcome {
                info!(
                    "push: dropping subscription {} ({}): {status}",
                    sub.id,
                    sub.user_agent.as_deref().unwrap_or("unknown device")
                );
            }
            let (id, o, now) = (sub.id, outcome.clone(), now_ms());
            self.store
                .exec(move |conn| settle(conn, id, &o, now))
                .await??;
            out.push((sub, outcome));
        }
        Ok(out)
    }

    fn send(&self, sub: &Subscription, plaintext: &[u8], urgent: bool, topic: &str) -> Outcome {
        match self.post(sub, plaintext, urgent, topic) {
            Ok((status, _)) if (200..300).contains(&status) => Outcome::Sent,
            Ok((status @ (404 | 410), _)) => Outcome::Dropped { status },
            Ok((status, text)) => Outcome::Failed {
                error: format!("status {status}: {text}"),
            },
            Err(e) => Outcome::Failed {
                error: format!("{e:#}"),
            },
        }
    }

    fn post(
        &self,
        sub: &Subscription,
        plaintext: &[u8],
        urgent: bool,
        topic: &str,
    ) -> Result<(u16, String)> {
        let ua_public = decode(&sub.p256dh)?;
        let auth = decode(&sub.auth)?;
        let body = encrypt(&ua_public, &auth, plaintext, &self.keys.rng)?;
        let authorization =
            self.keys
                .authorization(&sub.endpoint, &self.subject, now_ms() / 1000)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let response = agent
            .post(&sub.endpoint)
            .header("Authorization", &authorization)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", &self.ttl_secs.to_string())
            .header("Urgency", if urgent { "high" } else { "normal" })
            .header("Topic", topic)
            .send(body)?;
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().unwrap_or_default();
        Ok((status, truncate(text.trim(), 200)))
    }
}

/// Summarizes the outcomes as the notifier's result: an error names the
/// devices that failed, so it is logged once per distinct failure.
fn summarize(results: &[(Subscription, Outcome)]) -> Result<()> {
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(sub, outcome)| match outcome {
            Outcome::Failed { error } => Some(format!(
                "{} ({}): {error}",
                sub.id,
                sub.user_agent.as_deref().unwrap_or("unknown device")
            )),
            _ => None,
        })
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("{}", failed.join("; ")))
    }
}

impl Sink for Arc<Push> {
    const NAME: &'static str = "push";

    async fn publish(&self, note: &Note) -> Result<()> {
        let mut payload = serde_json::to_value(&note.content)?;
        payload["type"] = "show".into();
        payload["tag"] = note.seq.clone().into();
        payload["at"] = note.opened_at.into();
        summarize(
            &self
                .send_all(&payload, note.content.urgent, &topic(&note.seq))
                .await?,
        )
    }

    async fn delete(&self, seq: &str) -> Result<()> {
        let payload = json!({ "type": "close", "tag": seq });
        summarize(&self.send_all(&payload, false, &topic(seq)).await?)
    }
}

/// A plain message for the panel's "Send a test" button.
pub fn test_payload(url: &str) -> Value {
    json!({
        "type": "show",
        "tag": "test",
        "title": "ccdash",
        "body": "Push notifications reach this device.",
        "url": url,
        "urgent": false,
        "buttons": [],
        "more": false,
    })
}

/// Logs a failure of a one-off send.
pub fn log_failures(results: &[(Subscription, Outcome)]) {
    if let Err(e) = summarize(results) {
        warn!("push: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    /// RFC 8291 appendix A, from the shared secret on.
    #[test]
    fn matches_the_rfc_8291_example() {
        let ua_public = decode("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap();
        let as_public = decode("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8").unwrap();
        let auth = decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = decode("DGv6ra1nlYgDCS1FRnbzlw")
            .unwrap()
            .try_into()
            .unwrap();
        let shared = decode("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs").unwrap();
        let body = encrypt_with(
            &shared,
            &salt,
            &as_public,
            &ua_public,
            &auth,
            b"When I grow up, I want to be a watermelon",
        )
        .unwrap();
        assert_eq!(
            B64.encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    /// Encrypts for a fresh browser-side key and decrypts as the browser
    /// would, so the ECDH half of [`encrypt`] is covered too.
    #[test]
    fn round_trips_with_an_ephemeral_recipient() {
        let rng = SystemRandom::new();
        let ua_private =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua_private.compute_public_key().unwrap();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        let body = encrypt(ua_public.as_ref(), &auth, b"{\"type\":\"show\"}", &rng).unwrap();

        let (header, record) = body.split_at(21 + 65);
        let salt: [u8; 16] = header[..16].try_into().unwrap();
        assert_eq!(&header[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(header[20], 65);
        let as_public = &header[21..];
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let shared = agreement::agree_ephemeral(ua_private, &peer, |s| s.to_vec()).unwrap();
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(ua_public.as_ref());
        key_info.extend_from_slice(as_public);
        let ikm = expand(
            &hkdf::Salt::new(hkdf::HKDF_SHA256, &auth).extract(&shared),
            &key_info,
            32,
        )
        .unwrap();
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
        let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16).unwrap();
        let nonce = expand(&prk, b"Content-Encoding: nonce\0", 12).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut record = record.to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                aead::Aad::empty(),
                &mut record,
            )
            .unwrap();
        assert_eq!(plain, b"{\"type\":\"show\"}\x02");

        let big = vec![b'x'; MAX_PLAINTEXT + 1];
        assert!(encrypt(ua_public.as_ref(), &auth, &big, &rng).is_err());
    }

    #[test]
    fn vapid_tokens_verify_with_the_public_key() {
        let dir = std::env::temp_dir().join(format!("ccdash-push-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vapid.p8");
        let keys = Keys::load_or_create(&path).unwrap();
        let again = Keys::load_or_create(&path).unwrap();
        assert_eq!(keys.public_key(), again.public_key());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(decode(&keys.public_key()).unwrap().len(), 65);

        let header = keys
            .authorization(
                "https://push.example.org/send/abc?x=1",
                "mailto:me@example.org",
                1_700_000_000,
            )
            .unwrap();
        let (t, k) = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, keys.public_key());
        let [h, c, s] = t.split('.').collect::<Vec<_>>()[..] else {
            panic!("{t}")
        };
        let claims: Value = serde_json::from_slice(&decode(c).unwrap()).unwrap();
        assert_eq!(
            claims,
            json!({"aud": "https://push.example.org", "exp": 1_700_043_200, "sub": "mailto:me@example.org"})
        );
        let public = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, decode(k).unwrap());
        public
            .verify(format!("{h}.{c}").as_bytes(), &decode(s).unwrap())
            .unwrap();
        assert!(
            keys.authorization("http://plain.example.org/x", "x", 0)
                .is_err()
        );
        assert_eq!(
            origin("http://127.0.0.1:7870/push/1").unwrap(),
            "http://127.0.0.1:7870"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Serves one canned status and returns the raw request.
    fn capture(status: &'static str) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/push/one", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut got = Vec::new();
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = conn.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if got.len() >= end + 4 + len {
                    let body = got[end + 4..].to_vec();
                    got.truncate(end);
                    conn.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                    return (String::from_utf8_lossy(&got).into_owned(), body);
                }
            }
            panic!("no request")
        });
        (url, handle)
    }

    #[tokio::test]
    async fn sends_signed_encrypted_messages_and_drops_gone_subscriptions() {
        let dir = std::env::temp_dir().join(format!("ccdash-push-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let keys = Keys::load_or_create(&dir.join("vapid.p8")).unwrap();
        let cfg = config::Push {
            subject: Some("mailto:me@example.org".into()),
            ttl_secs: 60,
            ..config::Push::default()
        };
        let push = Push::new(keys, store.clone(), &cfg, "http://hub");
        let (url, server) = capture("201 Created");
        let sub = Subscribe {
            endpoint: url,
            keys: SubscribeKeys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            },
        };
        store
            .exec({
                let sub = sub.clone();
                move |conn| subscribe(conn, &sub, Some("Chrome"), 1).map(drop)
            })
            .await
            .unwrap()
            .unwrap();
        let payload = json!({"type": "show", "tag": "ask-1"});
        let results = push
            .send_all(&payload, true, &topic("ask-1"))
            .await
            .unwrap();
        assert_eq!(results[0].1, Outcome::Sent);
        let (head, body) = server.join().unwrap();
        assert!(head.starts_with("POST /push/one HTTP/1.1"), "{head}");
        let header = |name: &str| -> String {
            head.lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix(&format!("{name}:"))
                        .map(|_| l)
                })
                .map(|l| l.split_once(':').unwrap().1.trim().to_owned())
                .unwrap_or_default()
        };
        assert_eq!(header("content-encoding"), "aes128gcm");
        assert_eq!(header("ttl"), "60");
        assert_eq!(header("urgency"), "high");
        assert_eq!(header("topic"), topic("ask-1"));
        assert!(header("authorization").starts_with("vapid t="));
        // header, record: plaintext + delimiter + tag
        assert_eq!(body.len(), 21 + 65 + payload.to_string().len() + 1 + 16);
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        let listed = subscriptions(&store.reader().unwrap()).unwrap();
        assert!(listed[0].last_success_at.is_some());

        let (url, server) = capture("410 Gone");
        store
            .exec(move |conn| {
                conn.execute("UPDATE push_subscriptions SET endpoint = ?1", [url])
                    .map(drop)
            })
            .await
            .unwrap()
            .unwrap();
        let results = push
            .send_all(
                &json!({"type": "close", "tag": "ask-1"}),
                false,
                &topic("ask-1"),
            )
            .await
            .unwrap();
        assert_eq!(results[0].1, Outcome::Dropped { status: 410 });
        server.join().unwrap();
        assert!(subscriptions(&store.reader().unwrap()).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subscriptions_are_validated_stored_and_settled() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let sub = Subscribe {
            endpoint: "https://push.example.org/send/one".into(),
            keys: SubscribeKeys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4=".into(),
                auth: "BTBZMqHH6r4Tts7J+aSIgg".into(),
            },
        };
        sub.validate().unwrap();
        let bad = Subscribe {
            endpoint: "http://push.example.org/x".into(),
            ..sub.clone()
        };
        assert!(bad.validate().is_err());
        let bad = Subscribe {
            keys: SubscribeKeys {
                p256dh: "AAAA".into(),
                ..sub.keys.clone()
            },
            ..sub.clone()
        };
        assert!(bad.validate().is_err());

        let stored = subscribe(&conn, &sub, Some("Chrome"), 1_000).unwrap();
        assert_eq!((stored.id, stored.created_at), (1, 1_000));
        let again = subscribe(&conn, &sub, Some("Chrome 2"), 2_000).unwrap();
        assert_eq!(
            (again.id, again.created_at, again.user_agent.as_deref()),
            (1, 1_000, Some("Chrome 2"))
        );
        settle(
            &conn,
            1,
            &Outcome::Failed {
                error: "status 500".into(),
            },
            3_000,
        )
        .unwrap();
        let [s] = subscriptions(&conn).unwrap().try_into().unwrap();
        assert_eq!(
            (s.last_failure_at, s.last_failure.as_deref()),
            (Some(3_000), Some("status 500"))
        );
        settle(&conn, 1, &Outcome::Sent, 4_000).unwrap();
        let [s] = subscriptions(&conn).unwrap().try_into().unwrap();
        assert_eq!((s.last_success_at, s.last_failure), (Some(4_000), None));
        settle(&conn, 1, &Outcome::Dropped { status: 410 }, 5_000).unwrap();
        assert!(subscriptions(&conn).unwrap().is_empty());
        assert!(!unsubscribe(&conn, &sub.endpoint).unwrap());
        subscribe(&conn, &sub, None, 6_000).unwrap();
        assert!(unsubscribe(&conn, &sub.endpoint).unwrap());

        assert_eq!(topic("ask-1").len(), 32);
        assert!(
            topic("ask-1")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        );
        assert_ne!(topic("ask-1"), topic("ask-2"));
    }
}
