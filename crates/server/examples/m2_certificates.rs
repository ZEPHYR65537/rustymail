//! Disposable test certificates only. Refuses to overwrite any directory/file.
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use std::{fs, io::Write, path::Path};
fn save(root: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(root.join(name))?.write_all(bytes)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("a new test directory is required")?;
    let root = Path::new(&path);
    fs::create_dir(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name.push(
        rcgen::DnType::CommonName,
        "rustymail disposable laboratory CA",
    );
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate()?)?;
    save(root, "ca.pem", ca.pem().as_bytes())?;
    let key = KeyPair::generate()?;
    save(root, "server.key", key.serialize_pem().as_bytes())?;
    let wrong = KeyPair::generate()?;
    save(root, "wrong.key", wrong.serialize_pem().as_bytes())?;
    for name in ["server", "expired", "wrong-host", "rotated"] {
        let mut params = CertificateParams::new(vec![
            if name == "wrong-host" {
                "wrong.example.test"
            } else {
                "localhost"
            }
            .into(),
        ])?;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "rustymail disposable server");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        if name == "expired" {
            params.not_before = date_time_ymd(2000, 1, 1);
            params.not_after = date_time_ymd(2001, 1, 1);
        }
        let rotated;
        let signing = if name == "rotated" {
            rotated = KeyPair::generate()?;
            save(root, "rotated.key", rotated.serialize_pem().as_bytes())?;
            &rotated
        } else {
            &key
        };
        save(
            root,
            &format!("{name}.pem"),
            params.signed_by(signing, &ca)?.pem().as_bytes(),
        )?;
    }
    Ok(())
}
