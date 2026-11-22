//! Release signing for Windows updates (the private key never leaves this machine).
//!   cargo run --example sign_release -- keygen
//!   cargo run --example sign_release -- manifest VERSION ZIP OUTDIR [URL]   → latest.json(.sig)

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use parley::update::{Manifest, WindowsAsset};
use sha2::{Digest, Sha256};

fn key_path() -> PathBuf {
    let dir = std::env::var_os("PARLEY_RELEASE_KEYS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".config/parley-release"));
    dir.join("update-signing.key")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["keygen"] => {
            use std::io::Write;
            let path = key_path();
            std::fs::create_dir_all(path.parent().unwrap())?;
            let key = iroh::SecretKey::generate();
            // create_new: never overwrite an existing release key.
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut f = options.open(&path).with_context(|| format!("creating {}", path.display()))?;
            f.write_all(hex(&key.to_bytes()).as_bytes())?;
            println!("{}", key.public());
        }
        ["manifest", version, zip, out, rest @ ..] if rest.len() <= 1 => {
            let raw = std::fs::read_to_string(key_path()).context("reading the signing key")?;
            let bytes: Vec<u8> = (0..64).step_by(2).map(|i| u8::from_str_radix(&raw.trim()[i..i + 2], 16)).collect::<Result<_, _>>()?;
            let key = iroh::SecretKey::from_bytes(&bytes.try_into().map_err(|_| anyhow::anyhow!("bad key"))?);
            let data = std::fs::read(zip)?;
            let name = PathBuf::from(zip).file_name().unwrap().to_string_lossy().to_string();
            let m = Manifest {
                version: version.to_string(),
                windows_x64: WindowsAsset {
                    url: rest.first().map_or_else(|| format!("https://github.com/lpuq/parley/releases/download/v{version}/{name}"), |u| u.to_string()),
                    sha256: hex(&Sha256::digest(&data)),
                    size: data.len() as u64,
                },
            };
            let json = serde_json::to_vec_pretty(&m)?;
            let out = PathBuf::from(out);
            std::fs::write(out.join("latest.json"), &json)?;
            std::fs::write(out.join("latest.json.sig"), hex(&key.sign(&json).to_bytes()))?;
            println!("signed {version}");
        }
        _ => bail!("usage: sign_release keygen | manifest VERSION ZIP OUTDIR"),
    }
    Ok(())
}
