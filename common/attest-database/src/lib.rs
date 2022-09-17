use std::{error::Error, fs::Permissions, path::PathBuf, sync::Arc};

use attest_messages::{nonce::PrecomittedNonce, Envelope, Header, Unsigned};
use attest_util::ensure_dir;
use connection::MsgDB;
use ruma_serde::CanonicalJsonValue::Null;
use rusqlite::Connection;
use sapio_bitcoin::{
    secp256k1::{rand, Secp256k1, Signing},
    KeyPair,
};
use serde::Serialize;
use std::os::unix::fs::PermissionsExt;

pub mod connection;
pub mod db_handle;
pub mod sql_error;
pub mod sql_serializers;

#[cfg(test)]
mod tests;

pub async fn setup_db_at(dir: PathBuf, name: &str) -> Result<MsgDB, Box<dyn Error>> {
    tracing::debug!(
        "Request to Open Message DB at {} name {}",
        dir.display(),
        name
    );
    let dir: PathBuf = ensure_dir(dir, Some(Permissions::from_mode(0o700))).await?;
    let mut db_file = dir.clone();
    db_file.push(name);
    db_file.set_extension("sqlite3");
    tracing::debug!("Opening Message DB at: {}", db_file.display());
    let conn = Connection::open(db_file).unwrap();
    let mdb = MsgDB::new(Arc::new(tokio::sync::Mutex::new(conn)));
    mdb.get_handle().await.setup_tables();
    Ok(mdb)
}
pub async fn setup_db(application: &str, prefix: Option<PathBuf>) -> Result<MsgDB, Box<dyn Error>> {
    let dirs = directories::ProjectDirs::from("org", "judica", application).unwrap();
    let data_dir: PathBuf = dirs.data_dir().into();
    let data_dir = if let Some(prefix) = prefix {
        tracing::debug!("Creating DB with Prefix {}", prefix.display());
        prefix.join(data_dir.strip_prefix("/")?)
    } else {
        data_dir
    };
    setup_db_at(data_dir, "attestations").await
}

pub fn generate_new_user<C: Signing, T: Serialize>(
    secp: &Secp256k1<C>,
    init: Option<T>,
) -> Result<(KeyPair, PrecomittedNonce, Envelope), Box<dyn Error>> {
    let keypair: _ = KeyPair::new(secp, &mut rand::thread_rng());
    let nonce = PrecomittedNonce::new(secp);
    let next_nonce = PrecomittedNonce::new(secp);
    let sent_time_ms = attest_util::now();

    let pub_next_nonce = next_nonce.get_public(secp);
    let mut msg = Envelope::new(
        Header::new(
            keypair.public_key().x_only_public_key().0,
            pub_next_nonce,
            None,
            vec![],
            0,
            sent_time_ms,
            Unsigned::new(Default::default()),
            Default::default(),
        ),
        init.map(|v| ruma_serde::to_canonical_value(v))
            .unwrap_or(Ok(Null))?,
    );
    msg.sign_with(&keypair, secp, nonce)?;
    Ok((keypair, next_nonce, msg))
}
