//! CA bootstrap and on-the-fly leaf-certificate minting.
//!
//! On first run we generate a per-user root CA at `~/.pine/ca/root.{crt,key}`
//! and reuse it forever. Leaf certs for each upstream host are minted on demand
//! and cached in process memory keyed by SNI hostname.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot_compat::Mutex;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, Ia5String, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use time::{Duration, OffsetDateTime};

use crate::paths;

// rcgen 0.13 returns its `Certificate` and a separately-held `KeyPair`.
// We keep both so we can sign leaves later.
pub struct Ca {
    pub cert: rcgen::Certificate,
    pub key_pair: KeyPair,
    pub cert_pem: String,
    pub key_pem: String,
}

impl Ca {
    /// Load the CA from `~/.pine/ca/`, generating it on first run.
    pub fn load_or_generate() -> Result<Self> {
        let cert_path = paths::ca_cert_path()?;
        let key_path = paths::ca_key_path()?;

        if cert_path.exists() && key_path.exists() {
            check_key_perms(&key_path)?;
            let cert_pem = std::fs::read_to_string(&cert_path)
                .with_context(|| format!("reading {}", cert_path.display()))?;
            let key_pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("reading {}", key_path.display()))?;
            let key_pair =
                KeyPair::from_pem(&key_pem).context("parsing CA private key PEM")?;
            let params = CertificateParams::from_ca_cert_pem(&cert_pem)
                .context("parsing CA certificate PEM")?;
            let cert = params
                .self_signed(&key_pair)
                .context("re-binding loaded CA cert to its key")?;
            Ok(Self {
                cert,
                key_pair,
                cert_pem,
                key_pem,
            })
        } else {
            paths::ensure_dir(&paths::ca_dir()?)?;
            let ca = generate_root()?;
            write_secret(&key_path, ca.key_pem.as_bytes())?;
            std::fs::write(&cert_path, ca.cert_pem.as_bytes())
                .with_context(|| format!("writing {}", cert_path.display()))?;
            Ok(ca)
        }
    }
}

fn generate_root() -> Result<Ca> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "pine local root");
    dn.push(DnType::OrganizationName, "pine");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    params.not_after = now + Duration::days(365 * 10);

    let key_pair = KeyPair::generate().context("generating CA key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing CA cert")?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    Ok(Ca {
        cert,
        key_pair,
        cert_pem,
        key_pem,
    })
}

fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating secret file {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing secret file {}", path.display()))?;
    Ok(())
}

fn check_key_perms(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?;
        let mode = md.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(anyhow!(
                "CA private key {} has insecure mode {:o}; expected 0600",
                path.display(),
                mode
            ));
        }
    }
    let _ = path;
    Ok(())
}

/// Install the pine root CA into the OS trust store so Chromium-based
/// apps (Windsurf, Cursor, VS Code) accept our forged leaf certs. macOS uses
/// the user login keychain (no sudo). Linux/Windows print guidance only.
pub fn os_trust_install() -> Result<()> {
    let cert_path = paths::ca_cert_path()?;
    if !cert_path.exists() {
        let _ = Ca::load_or_generate()?;
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").context("$HOME not set")?;
        let keychain = format!("{home}/Library/Keychains/login.keychain-db");
        // NOTE: omit `-d` (admin domain) — that requires sudo and silently
        // installs the cert without an associated trust policy when run as
        // a normal user. User-domain trust works without sudo and is what
        // Chromium honors for user-added roots.
        let status = std::process::Command::new("/usr/bin/security")
            .args([
                "add-trusted-cert",
                "-r",
                "trustRoot",
                "-k",
                &keychain,
            ])
            .arg(&cert_path)
            .status()
            .context("running /usr/bin/security add-trusted-cert")?;
        if !status.success() {
            return Err(anyhow!(
                "security add-trusted-cert failed (exit {:?}). \
                 You may be prompted for your login keychain password.",
                status.code()
            ));
        }
        eprintln!(
            "pine: installed root CA into login keychain.\n\
             Untrust later with: pine ca untrust"
        );
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!(
            "Linux trust install is not automated yet. Two stores matter for Chromium apps:\n\n\
             1. NSS (per-user, no sudo):\n\
                certutil -d sql:$HOME/.pki/nssdb -A -t \"C,,\" -n 'pine Root CA' \\\n\
                    -i {cert}\n\n\
             2. System (Debian/Ubuntu, requires sudo):\n\
                sudo cp {cert} /usr/local/share/ca-certificates/pine.crt\n\
                sudo update-ca-certificates\n",
            cert = cert_path.display()
        );
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        eprintln!(
            "Windows: certutil -user -addstore Root \"{}\"",
            cert_path.display()
        );
        return Ok(());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(anyhow!("trust install not supported on this platform"))
    }
}

pub fn os_trust_remove() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let cert_path = paths::ca_cert_path()?;
        // -c matches by common name; our CA's CN is "pine local root".
        let status = std::process::Command::new("/usr/bin/security")
            .args(["delete-certificate", "-c", "pine local root"])
            .status()
            .context("running /usr/bin/security delete-certificate")?;
        if !status.success() {
            return Err(anyhow!(
                "security delete-certificate failed (exit {:?}). Cert at {}",
                status.code(),
                cert_path.display()
            ));
        }
        eprintln!("pine: removed root CA from login keychain.");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        eprintln!(
            "Linux: certutil -d sql:$HOME/.pki/nssdb -D -n 'pine Root CA'\n\
             and/or: sudo rm /usr/local/share/ca-certificates/pine.crt && sudo update-ca-certificates --fresh"
        );
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!("Windows: certutil -user -delstore Root \"pine local root\"");
        return Ok(());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(anyhow!("trust remove not supported on this platform"))
    }
}

/// Untrust AND delete the on-disk CA files. Best-effort on each step:
/// keychain removal failure is non-fatal (the cert may already be absent),
/// but file deletion errors propagate. After this, the next session mints a
/// fresh root CA.
pub fn reset() -> Result<()> {
    // Best-effort untrust — ignore errors (e.g. cert not in keychain).
    let _ = os_trust_remove();

    let cert = paths::ca_cert_path()?;
    let key = paths::ca_key_path()?;
    for p in [&cert, &key] {
        if p.exists() {
            std::fs::remove_file(p)
                .with_context(|| format!("removing {}", p.display()))?;
            eprintln!("pine: removed {}", p.display());
        }
    }
    eprintln!(
        "pine: CA reset complete. Next run will mint a fresh root CA — \
         re-run `pine ca trust` afterward."
    );
    Ok(())
}

/// Cheap heuristic: is the pine root CA trusted by the OS?
/// macOS: ask the keychain. Other platforms: return Ok(None) (unknown).
pub fn is_os_trusted() -> Result<Option<bool>> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/security")
            .args(["find-certificate", "-c", "pine local root"])
            .output()
            .context("running /usr/bin/security find-certificate")?;
        Ok(Some(out.status.success()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

/// In-memory cache of leaf certificates keyed by SNI host.
#[derive(Clone)]
pub struct LeafCertCache {
    ca: Arc<Ca>,
    inner: Arc<Mutex<HashMap<String, Arc<rustls::sign::CertifiedKey>>>>,
}

impl LeafCertCache {
    pub fn new(ca: Arc<Ca>) -> Self {
        Self {
            ca,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return a `CertifiedKey` for `host`, generating + caching it if missing.
    pub fn get_or_mint(&self, host: &str) -> Result<Arc<rustls::sign::CertifiedKey>> {
        if let Some(ck) = self.inner.lock().get(host).cloned() {
            return Ok(ck);
        }
        let ck = mint_leaf(&self.ca, host)?;
        let arc = Arc::new(ck);
        self.inner.lock().insert(host.to_string(), arc.clone());
        Ok(arc)
    }
}

fn mint_leaf(ca: &Ca, host: &str) -> Result<rustls::sign::CertifiedKey> {
    let san = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        SanType::IpAddress(ip)
    } else {
        SanType::DnsName(Ia5String::try_from(host.to_string()).context("invalid SNI host")?)
    };

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, host);
    params.distinguished_name = dn;
    params.subject_alt_names = vec![san];
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1);
    // Apple/Chromium reject TLS server leaf certs valid for >398 days.
    params.not_after = now + Duration::days(397);

    let leaf_key = KeyPair::generate().context("generating leaf key pair")?;
    let leaf_cert = params
        .signed_by(&leaf_key, &ca.cert, &ca.key_pair)
        .context("signing leaf cert with CA")?;

    let leaf_der = CertificateDer::from(leaf_cert.der().to_vec());
    let leaf_key_der = PrivateKeyDer::try_from(leaf_key.serialize_der())
        .map_err(|e| anyhow!("converting leaf key to DER: {e}"))?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&leaf_key_der)
        .context("turning leaf key into rustls signer")?;

    // Chain is just the leaf. Clients trust our CA directly via
    // NODE_EXTRA_CA_CERTS / SSL_CERT_FILE / BUN_CA_BUNDLE, so they will build
    // `leaf -> trusted-CA` without us shipping the CA in the chain.
    Ok(rustls::sign::CertifiedKey::new(
        vec![leaf_der],
        signing_key,
    ))
}

// Tiny shim so we can use a non-poisoning Mutex without pulling in parking_lot.
mod parking_lot_compat {
    use std::sync::{Mutex as StdMutex, MutexGuard};

    pub struct Mutex<T> {
        inner: StdMutex<T>,
    }

    impl<T> Mutex<T> {
        pub fn new(t: T) -> Self {
            Self {
                inner: StdMutex::new(t),
            }
        }
        pub fn lock(&self) -> MutexGuard<'_, T> {
            // Poisoning is irrelevant for this in-memory cache; recover transparently.
            match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            }
        }
    }
}
