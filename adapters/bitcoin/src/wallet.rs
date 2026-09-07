//! Watch-only wallets: what they are, and how one sync gathers everything
//! it needs before any diff is computed.
//!
//! **A resource is one WALLET -- a set of scriptPubKeys -- never one
//! address.** A wallet's balance and its history are properties of the
//! set; modelling each address as its own resource would make a
//! self-transfer look like a payment out and a payment in, and would make
//! "the balance" a number no resource holds.
//!
//! Address lists only. An xpub never leaves this adapter -- today because
//! there is no xpub support at all, and when it lands because derivation
//! happens here and only the derived addresses are ever queried. See
//! `PRIVACY.md`.

use crate::map::{is_txid, AddressStats, Tx};
use crate::source::{FetchError, Source};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use sumer_wire::ProviderDetail;

/// One watch-only wallet.
#[derive(Debug, Clone)]
pub struct Wallet {
    /// **Immutable.** Changing it creates a NEW resource: the host keys
    /// resources by `(adapter_id, resource_id)` (`spec/wire.md` 10), so a
    /// rename forks the observation chain. This adapter derives no stable
    /// key from the address set to paper over that -- see README.
    pub resource_id: String,
    pub label: String,
    /// Sorted and deduplicated: a duplicate address would double-count the
    /// balance, since Esplora's counters are per scriptPubKey.
    pub addresses: Vec<String>,
    pub owned: BTreeSet<String>,
    /// SHA-256 of the address set. A change means the cached balances on
    /// disk are a different address set's figures.
    pub address_hash: String,
}

#[derive(Deserialize)]
struct ConfigFile {
    wallets: Vec<WalletFile>,
}

#[derive(Deserialize)]
struct WalletFile {
    resource_id: String,
    #[serde(default)]
    label: Option<String>,
    addresses: Vec<String>,
}

/// Reads and validates the wallet file. Every rejection here is a trust
/// boundary: a `resource_id` becomes a state-file name and an address
/// becomes part of a request path, so neither is taken on faith.
pub fn load(path: &Path) -> Result<Vec<Wallet>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: ConfigFile =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    if file.wallets.is_empty() {
        return Err(format!("{}: no wallets configured", path.display()));
    }

    let mut out: Vec<Wallet> = Vec::with_capacity(file.wallets.len());
    for w in file.wallets {
        if w.resource_id.is_empty()
            || w.resource_id.len() > 64
            || w.resource_id == "."
            || w.resource_id == ".."
            || !w
                .resource_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(format!(
                "resource_id {:?}: must be 1-64 bytes of [A-Za-z0-9._-] and not \".\" or \"..\" \
                 (it names a file under --state-dir)",
                w.resource_id
            ));
        }
        if out.iter().any(|o| o.resource_id == w.resource_id) {
            return Err(format!("resource_id {:?} appears twice", w.resource_id));
        }
        if w.addresses.is_empty() {
            return Err(format!("{}: no addresses", w.resource_id));
        }
        for a in &w.addresses {
            // Every Bitcoin address encoding in use -- base58check and
            // bech32/bech32m -- is ASCII alphanumeric end to end. Anything
            // else would be interpolated into a URL path and a corpus file
            // name, so it does not get in.
            if a.len() < 10 || a.len() > 100 || !a.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(format!(
                    "{}: address {a:?} is not 10-100 ASCII alphanumeric bytes",
                    w.resource_id
                ));
            }
        }
        let owned: BTreeSet<String> = w.addresses.iter().cloned().collect();
        if owned.len() != w.addresses.len() {
            return Err(format!(
                "{}: the same address is listed twice, which would double-count the balance",
                w.resource_id
            ));
        }
        let addresses: Vec<String> = owned.iter().cloned().collect();
        // Sorted and newline-joined so the hash depends on the SET, not on
        // the order the file happened to list it in.
        let address_hash = sha256_hex(addresses.join("\n").as_bytes());
        out.push(Wallet {
            label: w.label.unwrap_or_else(|| w.resource_id.clone()),
            resource_id: w.resource_id,
            addresses,
            owned,
            address_hash,
        });
    }
    Ok(out)
}

/// Everything one sync fetched. Built only when EVERY fetch succeeded --
/// see [`sync`].
pub struct ChainData {
    pub txs: BTreeMap<String, Tx>,
    pub mempool: BTreeSet<String>,
}

/// Fetches one wallet's whole current state: what the provider says NOW.
///
/// **Any failure anywhere returns `Err` and the caller emits no
/// observations at all.** There is no partial `ChainData`: half a wallet's
/// history reported as if it were the whole of it is a claim this adapter
/// has no evidence for.
pub fn sync(source: &Source, wallet: &Wallet) -> Result<ChainData, FetchError> {
    let mut txs: BTreeMap<String, Tx> = BTreeMap::new();
    for address in &wallet.addresses {
        let confirmed = source.address_chain(address)?;
        let pending = source.address_mempool(address)?;
        for tx in confirmed.into_iter().chain(pending) {
            if !is_txid(&tx.txid) {
                return Err(FetchError::Unavailable {
                    detail: ProviderDetail {
                        code: "malformed_txid".to_owned(),
                        message: format!(
                            "{address}: listing carried a txid that is not 64 hex characters"
                        ),
                        raw: serde_json::Value::String(tx.txid.chars().take(128).collect()),
                    },
                });
            }
            // The same transaction can arrive twice across addresses, and
            // can confirm between two calls: the confirmed copy wins, so
            // a mid-sync confirmation is never recorded as pending.
            if tx.height().is_some() || !txs.contains_key(&tx.txid) {
                txs.insert(tx.txid.clone(), tx);
            }
        }
    }
    let mempool: BTreeSet<String> = txs
        .values()
        .filter(|tx| tx.height().is_none())
        .map(|tx| tx.txid.clone())
        .collect();

    Ok(ChainData { txs, mempool })
}

/// The wallet's two balances: `(confirmed, unconfirmed)`, in satoshis.
///
/// Esplora exposes no balance field; these are funded minus spent, summed
/// over the address set. The unconfirmed figure may be NEGATIVE -- a
/// wallet spending unconfirmed change has a mempool that records the spend
/// of a confirmed output it never funded.
pub fn balances(source: &Source, wallet: &Wallet) -> Result<(i128, i128), FetchError> {
    let mut confirmed: i128 = 0;
    let mut unconfirmed: i128 = 0;
    for address in &wallet.addresses {
        let AddressStats {
            chain_stats,
            mempool_stats,
        } = source.address_stats(address)?;
        confirmed += chain_stats.net();
        unconfirmed += mempool_stats.net();
    }
    Ok((confirmed, unconfirmed))
}

// ---------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256, hex-encoded lowercase.
///
/// Hand-written rather than adding a hash dependency for the one use this
/// crate has: fingerprinting a wallet's address set so the balance cache
/// can tell one wallet's figures from another's. Pinned against the published
/// FIPS 180-4 test vectors below.
#[must_use]
pub fn sha256_hex(msg: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    // An address set is bytes on disk, never 2^61 of them; the saturating
    // fallback exists so this function has no panic path at all.
    let bit_len = u64::try_from(msg.len()).unwrap_or(u64::MAX).wrapping_mul(8);

    let mut padded = msg.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap_or([0; 4]));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Two blocks, exercising the message schedule across a boundary.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn the_address_hash_depends_on_the_set_not_the_order() {
        let a = sha256_hex(["a", "b"].join("\n").as_bytes());
        let b = sha256_hex(["b", "a"].join("\n").as_bytes());
        assert_ne!(a, b, "the hash is over the joined string...");
        // ...which is why `load` sorts before joining. Same set, either
        // order in the file, one hash:
        let one = write_config(
            r#"{"wallets":[{"resource_id":"w","addresses":["bc1qaaaaaaaaa","bc1qbbbbbbbbb"]}]}"#,
        );
        let two = write_config(
            r#"{"wallets":[{"resource_id":"w","addresses":["bc1qbbbbbbbbb","bc1qaaaaaaaaa"]}]}"#,
        );
        assert_eq!(
            load(&one).unwrap()[0].address_hash,
            load(&two).unwrap()[0].address_hash
        );
    }

    fn write_config(contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sumer-btc-cfg-{}-{}.json",
            std::process::id(),
            sha256_hex(contents.as_bytes())
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn config_rejects_what_would_escape_a_path_or_double_count() {
        for bad in [
            r#"{"wallets":[]}"#,
            r#"{"wallets":[{"resource_id":"../etc","addresses":["bc1qaaaaaaaaa"]}]}"#,
            r#"{"wallets":[{"resource_id":"a/b","addresses":["bc1qaaaaaaaaa"]}]}"#,
            r#"{"wallets":[{"resource_id":"w","addresses":[]}]}"#,
            r#"{"wallets":[{"resource_id":"w","addresses":["bc1q/../../x"]}]}"#,
            r#"{"wallets":[{"resource_id":"w","addresses":["bc1qaaaaaaaaa","bc1qaaaaaaaaa"]}]}"#,
            r#"{"wallets":[{"resource_id":"w","addresses":["bc1qaaaaaaaaa"]},{"resource_id":"w","addresses":["bc1qbbbbbbbbb"]}]}"#,
        ] {
            let path = write_config(bad);
            assert!(load(&path).is_err(), "must be rejected: {bad}");
        }
    }

    #[test]
    fn config_accepts_a_wallet_and_defaults_its_label() {
        let path =
            write_config(r#"{"wallets":[{"resource_id":"cold","addresses":["bc1qaaaaaaaaa"]}]}"#);
        let wallets = load(&path).unwrap();
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].label, "cold");
        assert_eq!(wallets[0].owned.len(), 1);
    }
}
