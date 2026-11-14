use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
};
use tempfile::NamedTempFile;

use crate::config::HttpTls;

#[derive(Default)]
pub struct ServerTls {
    pub config: Option<RustlsConfig>,
    pub generated: bool,
}

pub fn server(
    settings: Option<&HttpTls>,
    default_name: Option<String>,
    local_directory: &Path,
) -> Result<ServerTls> {
    let Some(settings) = settings else {
        return Ok(ServerTls::default());
    };
    let (cert, key, generated) = match (&settings.cert_file, &settings.key_file) {
        (Some(cert), Some(key)) => (cert.clone(), key.clone(), false),
        (None, None) => {
            let mut names = settings.server_names.clone();
            if names.is_empty() {
                names.push(default_name.unwrap_or_else(|| "localhost".into()));
            }
            let (cert, key) = local_paths(local_directory);
            ensure_local_pair(&cert, &key, names)?;
            (cert, key, true)
        }
        _ => anyhow::bail!("HTTPS requires both certFile and keyFile"),
    };
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates(&cert)?, private_key(&key)?)
        .context("HTTPS certificate and key could not be loaded together")?;
    // HTTP/1.1 serves both the JSON API and WebSocket upgrades.
    Ok(ServerTls {
        config: Some(RustlsConfig::from_config(Arc::new(config))),
        generated,
    })
}

pub fn mqtt_transport(ca_file: Option<&Path>) -> Result<rumqttc::Transport> {
    let mut roots = RootCertStore::empty();
    if let Some(path) = ca_file {
        // Explicit trust for a private broker is scoped to this MQTT client.
        for certificate in certificates(path)? {
            roots
                .add(certificate)
                .context("invalid MQTT trust certificate")?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        ensure!(
            !roots.is_empty(),
            "no usable system trust certificates for MQTT TLS"
        );
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(rumqttc::Transport::tls_with_config(config.into()))
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut input =
        BufReader::new(File::open(path).context("could not open PEM certificate file")?);
    let certificates = rustls_pemfile::certs(&mut input)
        .collect::<std::io::Result<Vec<_>>>()
        .context("could not parse PEM certificates")?;
    ensure!(
        !certificates.is_empty(),
        "certificate file has no PEM certificates"
    );
    Ok(certificates)
}

fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut input =
        BufReader::new(File::open(path).context("could not open HTTPS private key file")?);
    rustls_pemfile::private_key(&mut input)
        .context("could not parse HTTPS private key")?
        .context("key file has no PEM private key")
}

fn local_paths(directory: &Path) -> (PathBuf, PathBuf) {
    (directory.join("server.crt"), directory.join("server.key"))
}

fn ensure_local_pair(cert: &Path, key: &Path, names: Vec<String>) -> Result<()> {
    let directory = cert
        .parent()
        .context("missing HTTPS certificate directory")?;
    create_private_directory(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(".generate.lock"))
        .context("could not open the HTTPS certificate generation lock")?;
    lock.lock()
        .context("could not lock the HTTPS certificate directory")?;
    match (cert.try_exists()?, key.try_exists()?) {
        (true, true) => return Ok(()),
        (false, false) => {}
        _ => anyhow::bail!("locally generated HTTPS certificate pair is incomplete"),
    }

    let mut settings = CertificateParams::new(names).context("invalid HTTPS certificate names")?;
    let now = time::OffsetDateTime::now_utc();
    settings.not_before = now - time::Duration::minutes(5);
    settings.not_after = now + time::Duration::days(365);
    settings.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let private = KeyPair::generate().context("could not generate HTTPS private key")?;
    let signed = settings
        .self_signed(&private)
        .context("could not sign HTTPS certificate")?;
    let mut certificate_file = NamedTempFile::new_in(directory)?;
    certificate_file.write_all(signed.pem().as_bytes())?;
    certificate_file.as_file().sync_all()?;
    let mut key_file = NamedTempFile::new_in(directory)?;
    key_file.write_all(private.serialize_pem().as_bytes())?;
    key_file.as_file().sync_all()?;
    key_file
        .persist_noclobber(key)
        .map_err(|error| error.error)?;
    if let Err(error) = certificate_file.persist_noclobber(cert) {
        let _ = fs::remove_file(key);
        return Err(error.error).context("could not save the generated HTTPS certificate");
    }
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("could not create the HTTPS certificate directory")
}
