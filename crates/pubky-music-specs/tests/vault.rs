//! The sources vault (VAULT.md): the v1 test vectors, the refusals, the device list, and a client
//! proof checked by the pubky SDK's own verifier.
//!
//! `VAULT_WRITE_VECTORS=1 cargo test -p pubky-music-specs --features vault --test vault` rewrites
//! `tests/vectors/vault-v1.json` from the fixed inputs below; without it the file is the reference.
#![cfg(feature = "vault")]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use pubky_music_specs::vault::*;
use serde_json::{json, Value};

const OWNER: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";
const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/vault-v1.json");

fn bytes<const N: usize>(fill: u8) -> [u8; N] {
    let mut b = [0u8; N];
    for (i, x) in b.iter_mut().enumerate() {
        *x = fill.wrapping_add(i as u8);
    }
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The plaintext of the vectors: exact bytes, so they go in the file as a string.
const SIGN_IN: &str = r#"{"service":"tidal","label":"listener@example.org","way":"built-in","client_id":"example-client","tokens":{"access_token":"at-1","refresh_token":"rt-1","token_type":"Bearer"},"scopes":["r_usr","w_usr"],"expires_at":1761403600,"revision":1,"settings":{"quality":"lossless"}}"#;

struct Case {
    name: &'static str,
    epoch: u64,
    clients: Vec<u8>,
}

fn build() -> Value {
    let path = blob_path("tidal").unwrap();
    let secrets: Vec<ClientSecret> = [0x11u8, 0x22]
        .iter()
        .map(|f| ClientSecret::from_bytes(bytes(*f)))
        .collect();
    let data_key = DataKey::from_bytes(bytes(0x40));
    let nonce: [u8; 24] = bytes(0x60);
    let cases = [
        Case {
            name: "two clients",
            epoch: 1,
            clients: vec![0, 1],
        },
        Case {
            name: "after signing out the second client: rotated to epoch 2",
            epoch: 2,
            clients: vec![0],
        },
    ];
    let out: Vec<Value> = cases
        .iter()
        .map(|c| {
            let recipients: Vec<(ClientKey, [u8; 32])> = c
                .clients
                .iter()
                .map(|i| {
                    (
                        secrets[*i as usize].public(),
                        bytes(0x80 + 0x10 * *i + c.epoch as u8),
                    )
                })
                .collect();
            let key = if c.epoch == 1 {
                data_key.clone()
            } else {
                DataKey::from_bytes(bytes(0x50))
            };
            let env = seal_with(
                OWNER,
                &path,
                c.epoch,
                &key,
                &nonce,
                SIGN_IN.as_bytes(),
                &recipients,
            )
            .unwrap();
            json!({
                "name": c.name,
                "epoch": c.epoch,
                "data_key": hex(&if c.epoch == 1 { bytes::<32>(0x40) } else { bytes::<32>(0x50) }),
                "nonce": hex(&nonce),
                "ephemeral_secrets": recipients.iter().map(|(_, e)| hex(e)).collect::<Vec<_>>(),
                "envelope": env,
            })
        })
        .collect();
    json!({
        "about": "pubky-music-specs VAULT.md v1. Hex for raw inputs; the envelope as written to the homeserver.",
        "owner": OWNER,
        "path": path,
        "clients": secrets.iter().map(|s| json!({"secret": hex(&s.to_bytes()), "client_key": s.public()})).collect::<Vec<_>>(),
        "plaintext": SIGN_IN,
        "cases": out,
    })
}

#[test]
fn vectors_v1() {
    let built = build();
    if std::env::var_os("VAULT_WRITE_VECTORS").is_some() {
        std::fs::write(VECTORS, serde_json::to_string_pretty(&built).unwrap() + "\n").unwrap();
    }
    let file: Value = serde_json::from_str(&std::fs::read_to_string(VECTORS).unwrap()).unwrap();
    assert_eq!(
        built, file,
        "the reference code no longer produces tests/vectors/vault-v1.json"
    );

    // Every listed recipient opens every case; the plaintext comes back byte for byte
    let path = file["path"].as_str().unwrap();
    for case in file["cases"].as_array().unwrap() {
        let env: Envelope = serde_json::from_value(case["envelope"].clone()).unwrap();
        for c in file["clients"].as_array().unwrap() {
            let secret =
                ClientSecret::from_bytes(ClientKey::from_hex(c["secret"].as_str().unwrap()).unwrap().0);
            let key: ClientKey = serde_json::from_value(c["client_key"].clone()).unwrap();
            match open(OWNER, path, &env, &secret) {
                Ok((_, plain)) => {
                    assert!(env.has_recipient(&key));
                    assert_eq!(plain.as_slice(), SIGN_IN.as_bytes());
                }
                Err(e) => {
                    assert!(!env.has_recipient(&key), "{}: {e}", case["name"]);
                    assert_eq!(e, VaultError::NotARecipient);
                }
            }
        }
    }
}

#[test]
fn x25519_is_rfc_7748() {
    // RFC 7748 §6.1, Alice
    let alice =
        ClientKey::from_hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap();
    assert_eq!(
        ClientSecret::from_bytes(alice.0).public().to_hex(),
        "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
    );
}

#[test]
fn what_does_not_open() {
    let path = blob_path("qobuz").unwrap();
    let a = ClientSecret::generate();
    let b = ClientSecret::generate();
    let key = DataKey::generate();
    let env = seal(OWNER, &path, 4, &key, br#"{"service":"qobuz"}"#, &[a.public()]).unwrap();
    assert!(open(OWNER, &path, &env, &a).is_ok());
    // Not a recipient
    assert_eq!(
        open(OWNER, &path, &env, &b).unwrap_err(),
        VaultError::NotARecipient
    );
    // Bound to the owner, the path and the epoch
    assert_eq!(
        open("someone-else", &path, &env, &a).unwrap_err(),
        VaultError::Decrypt
    );
    assert_eq!(
        open(OWNER, &blob_path("tidal").unwrap(), &env, &a).unwrap_err(),
        VaultError::Decrypt
    );
    let mut older = env.clone();
    older.epoch = 3;
    assert_eq!(open(OWNER, &path, &older, &a).unwrap_err(), VaultError::Decrypt);
    // A changed ciphertext, a changed wrapped key
    let mut ct = env.clone();
    let mut raw = URL_SAFE_NO_PAD.decode(&ct.ciphertext).unwrap();
    raw[0] ^= 1;
    ct.ciphertext = URL_SAFE_NO_PAD.encode(raw);
    assert_eq!(open(OWNER, &path, &ct, &a).unwrap_err(), VaultError::Decrypt);
    let mut wk = env.clone();
    let mut raw = URL_SAFE_NO_PAD.decode(&wk.recipients[0].wrapped_key).unwrap();
    raw[5] ^= 1;
    wk.recipients[0].wrapped_key = URL_SAFE_NO_PAD.encode(raw);
    assert_eq!(open(OWNER, &path, &wk, &a).unwrap_err(), VaultError::Decrypt);
    // A recipient's wrap moved to another client's entry doesn't open for that client
    let mut moved = seal(OWNER, &path, 4, &key, b"{}", &[a.public(), b.public()]).unwrap();
    moved.recipients[1].epk = moved.recipients[0].epk.clone();
    moved.recipients[1].wrapped_key = moved.recipients[0].wrapped_key.clone();
    assert_eq!(open(OWNER, &path, &moved, &b).unwrap_err(), VaultError::Decrypt);
    // Low-order keys are refused
    assert_eq!(
        seal(OWNER, &path, 1, &key, b"{}", &[ClientKey([0; 32])]).unwrap_err(),
        VaultError::WeakKey
    );
    // Another version
    let mut v2 = serde_json::to_value(&env).unwrap();
    v2["version"] = json!(2);
    assert_eq!(
        Envelope::parse(&serde_json::to_vec(&v2).unwrap()).unwrap_err(),
        VaultError::UnsupportedVersion(2)
    );
    assert_eq!(
        Envelope::parse(&vec![b' '; MAX_BLOB_BYTES + 1]).unwrap_err(),
        VaultError::TooLarge
    );
}

#[test]
fn adding_a_device_keeps_the_epoch_and_the_content() {
    let path = blob_path("spotify").unwrap();
    let a = ClientSecret::generate();
    let b = ClientSecret::generate();
    let mut env = seal(
        OWNER,
        &path,
        7,
        &DataKey::generate(),
        b"{\"service\":\"spotify\"}",
        &[a.public()],
    )
    .unwrap();
    let before = env.ciphertext.clone();
    add_recipient(OWNER, &path, &mut env, &a, &b.public()).unwrap();
    add_recipient(OWNER, &path, &mut env, &a, &b.public()).unwrap(); // once only
    assert_eq!(env.recipients.len(), 2);
    assert_eq!((env.epoch, env.ciphertext.as_str()), (7, before.as_str()));
    assert_eq!(
        open(OWNER, &path, &env, &b).unwrap().1.as_slice(),
        b"{\"service\":\"spotify\"}"
    );
    // A client that can't open the blob can't add anyone
    let c = ClientSecret::generate();
    assert_eq!(
        add_recipient(OWNER, &path, &mut env, &c, &c.public()).unwrap_err(),
        VaultError::NotARecipient
    );
}

#[test]
fn sign_in_keeps_what_it_does_not_know_and_hides_tokens() {
    let mut v: Value = serde_json::from_str(SIGN_IN).unwrap();
    v["from_a_newer_client"] = json!({"x": 1});
    let s: SignIn = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    assert!(!format!("{s:?}").contains("rt-1"));
    assert!(s.needs_refresh(1761403600 - 30));
    assert!(!s.needs_refresh(1761403600 - 120));
    assert!(!format!("{:?}", ClientSecret::generate()).contains(|c: char| c.is_ascii_digit()));
}

#[test]
fn paths_and_capabilities() {
    assert_eq!(
        blob_path("tidal").as_deref(),
        Some("/priv/pubky.music/vault/tidal.json")
    );
    assert_eq!(
        lease_path("tidal").as_deref(),
        Some("/priv/pubky.music/vault/tidal.lease.json")
    );
    for bad in ["", "index", "Tidal", "../index", "a/b", "x.json", &"a".repeat(33)] {
        assert!(blob_path(bad).is_none(), "{bad}");
    }
    for ok in [
        "/:rw",
        "/priv/:rw",
        "/priv/pubky.music/:rwe",
        "/priv/pubky.music/vault/:wr",
    ] {
        assert!(covers_vault(ok), "{ok}");
    }
    for no in [
        "/priv/pubky.music/vault/:r",
        "/priv/pubky.music/vault/tidal.json:rw",
        "/pub/:rw",
        "/priv/pubky.music/vaults/:rw",
        "/priv/pubky.music/vault/:e",
    ] {
        assert!(!covers_vault(no), "{no}");
    }
}

#[test]
fn index_round_trip_and_revocation() {
    let a = ClientSecret::from_bytes(bytes(0x11)).public();
    let raw = json!({
        "version": 1,
        "clients": [{"client_key": a, "name": "Living room node", "authorised_at": 1761400000,
                     "last_used_at": 1761400000, "proof": {"grant": "g", "pop": "p"}, "future": true}],
        "revoked": [{"grant_id": "old-grant", "at": 1761500000}],
        "note": "kept"
    });
    let ix = Index::parse(&serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&ix).unwrap(), raw);
    assert!(ix.is_revoked(&ClientKey([9; 32]), Some("old-grant")));
    assert!(!ix.is_revoked(&a, Some("new-grant")));
    assert!(!ix.is_revoked(&a, None));
    let mut v3 = raw.clone();
    v3["version"] = json!(3);
    assert_eq!(
        Index::parse(&serde_json::to_vec(&v3).unwrap()).unwrap_err(),
        VaultError::UnsupportedVersion(3)
    );
}

/// A client's proof made the way the SDK's `create_custom_pop` makes it, then checked with the SDK's
/// own `verify_custom_grant_pop` and [`client_entry_problem`].
#[test]
fn a_client_proof_verifies_with_the_pubky_sdk() {
    use pubky::custom_pop::{
        verify_custom_grant_pop, CustomPop, CUSTOM_POP_JWS_TYP, DEFAULT_CUSTOM_POP_CLOCK_SKEW,
    };
    use pubky::{Capability, ClientId, GrantClaims, GrantId, Keypair, GRANT_JWS_TYP};

    let identity = Keypair::from_secret(&bytes(0xa0));
    let grant_client = Keypair::from_secret(&bytes(0xb0));
    let vault_key = ClientSecret::from_bytes(bytes(0x11)).public();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = |caps: &[&str]| GrantClaims {
        iss: identity.public_key(),
        client_id: ClientId::new("music.pubky.app").unwrap(),
        caps: caps.iter().map(|c| c.parse::<Capability>().unwrap()).collect(),
        cnf: grant_client.public_key(),
        jti: GrantId::parse("vaultgrant01").unwrap(),
        iat: now - 10,
        exp: now + 3600,
    };
    let pop = |data: Value| {
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "EdDSA", "typ": CUSTOM_POP_JWS_TYP}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(
            json!({"gid": "vaultgrant01", "iat": now - 5, "nonce": "vaultnonce01", "data": data}).to_string(),
        );
        let input = format!("{header}.{payload}");
        let sig = grant_client.sign(input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    };
    let data = serde_json::to_value(ClientProofData::new(vault_key, "Living room node")).unwrap();
    let entry = ClientEntry {
        client_key: vault_key,
        name: "Living room node".into(),
        authorised_at: now,
        last_used_at: now,
        proof: Proof {
            grant: claims(&["/priv/pubky.music/:rw"]).sign(&identity, GRANT_JWS_TYP),
            pop: pop(data),
        },
        extra: Default::default(),
    };
    let check = |e: &ClientEntry| {
        let creds = CustomPop {
            grant: e.proof.grant.clone(),
            pop: e.proof.pop.clone(),
        };
        let v = verify_custom_grant_pop(&creds, DEFAULT_CUSTOM_POP_CLOCK_SKEW).map_err(|e| e.to_string())?;
        let caps: Vec<String> = v.grant_claims().caps.iter().map(|c| c.to_string()).collect();
        match client_entry_problem(
            e,
            &identity.public_key().z32(),
            &v.identity().z32(),
            &caps,
            v.data(),
        ) {
            Some(p) => Err(p.to_string()),
            None => Ok(()),
        }
    };
    assert_eq!(check(&entry), Ok(()));

    // The proof is for one key: another entry can't borrow it
    let mut other = entry.clone();
    other.client_key = ClientSecret::from_bytes(bytes(0x22)).public();
    assert_eq!(check(&other), Err("the proof is for another key".into()));
    // A grant without the vault
    let mut narrow = entry.clone();
    narrow.proof.grant = claims(&["/pub/pubky.music/:rw"]).sign(&identity, GRANT_JWS_TYP);
    assert!(check(&narrow).unwrap_err().contains("doesn't cover the vault"));
    // A grant signed by someone else, claiming to be the owner: the SDK refuses it
    let mut forged = entry.clone();
    forged.proof.grant = claims(&["/priv/:rw"]).sign(&Keypair::from_secret(&bytes(0xc0)), GRANT_JWS_TYP);
    assert!(check(&forged).is_err());
    // Proof data of another kind
    let mut kind = entry.clone();
    kind.proof.pop = pop(json!({"kind": "paykit", "v": 1, "client_key": vault_key, "name": "x"}));
    assert_eq!(
        check(&kind),
        Err("the proof's data isn't a vault client's".into())
    );
}
