use screentime_server::{AppState, config::Config, db, open_db, router};
use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal};

const USAGE: &str = "usage:
  screentime-server [serve]          run the server (configured by SCREENTIME_* variables)
  screentime-server hash-password    read a password and print its hash for SCREENTIME_ADMIN_PASSWORD_HASH
  screentime-server gen-cert DIR NAME...  make a private CA and a server certificate for NAME (host names / IPs)
  screentime-server fingerprint CERT.pem  print a certificate's fingerprint for clients' serverFingerprint
  screentime-server add-client HOST  register a machine and print its token (replaces any old one)
  screentime-server list-clients";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => serve().await,
        ["hash-password"] => {
            let password = if std::io::stdin().is_terminal() {
                let p = rpassword::prompt_password("Admin password: ")?;
                if p != rpassword::prompt_password("Again: ")? {
                    anyhow::bail!("passwords don't match");
                }
                p
            } else {
                std::io::stdin().lock().lines().next().context("no password on stdin")??
            };
            let min = screentime_server::admin::MIN_PASSWORD_CHARS;
            if password.chars().count() < min {
                anyhow::bail!("use at least {min} characters");
            }
            println!("{}", screentime_server::auth::hash_password(&password)?);
            Ok(())
        }
        ["add-client", host] => {
            let config = Config::from_env()?;
            let pool = open_db(&config.db_url).await?;
            println!("{}", db::add_client(&pool, host).await?);
            Ok(())
        }
        ["gen-cert", dir, names @ ..] => {
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            screentime_server::certs::generate(std::path::Path::new(dir), &names)
        }
        ["fingerprint", cert] => {
            println!("{}", screentime_server::certs::pem_fingerprint(std::path::Path::new(cert))?);
            Ok(())
        }
        ["list-clients"] => {
            let config = Config::from_env()?;
            let pool = open_db(&config.db_url).await?;
            for c in db::list_clients(&pool).await? {
                println!("{}\t{}", c.hostname, c.last_seen.as_deref().unwrap_or("never"));
            }
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let config = Config::from_env()?;
    if config.admin_password_hash.is_none() {
        tracing::warn!("no admin password set; the web UI can't be signed into");
    }

    let pool = open_db(&config.db_url).await?;
    let addr: std::net::SocketAddr = config.listen.parse().context("SCREENTIME_LISTEN")?;
    let tls = config.tls.clone();
    let state = AppState::new(pool, config);
    tokio::spawn(screentime_server::notify::follow_replies(state.clone()));
    tokio::spawn(screentime_server::wifi::run(state.clone()));
    let app = router(state).into_make_service();

    match tls {
        Some((cert, key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading TLS cert {} and key {}", cert.display(), key.display()))?;
            // Certificates are renewed in place (Let's Encrypt every couple of months); pick up
            // the new one without a restart.
            {
                let (tls, cert, key) = (tls.clone(), cert.clone(), key.clone());
                tokio::spawn(async move {
                    let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
                    let mut seen = modified(&cert);
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                        let now = modified(&cert);
                        if now == seen {
                            continue;
                        }
                        match tls.reload_from_pem_file(&cert, &key).await {
                            Ok(()) => {
                                tracing::info!("reloaded the renewed TLS certificate");
                                seen = now;
                            }
                            Err(e) => tracing::warn!("reloading the TLS certificate failed: {e:#}"),
                        }
                    }
                });
            }
            match screentime_server::certs::pem_fingerprint(&cert) {
                Ok(f) => tracing::info!("certificate fingerprint (for clients' serverFingerprint): {f}"),
                Err(e) => tracing::warn!("couldn't fingerprint the certificate: {e:#}"),
            }
            tracing::info!("listening on https://{addr}");
            axum_server::bind_rustls(addr, tls).serve(app).await?;
        }
        None => {
            tracing::warn!("no TLS certificate set; serving plain HTTP, which is only safe behind a TLS proxy");
            tracing::info!("listening on http://{addr}");
            axum_server::bind(addr).serve(app).await?;
        }
    }
    Ok(())
}
