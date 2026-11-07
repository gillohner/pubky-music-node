"""Usage: python3 -I check_vault_v2.py vault-v2.json (needs the `cryptography` package).

Independent check of VAULT.md v2 vectors: the signing key derived from the client's X25519 secret
(HKDF-SHA-256), the writer's signature (Ed25519) over the signed data, built here from the
envelope, nothing from the Rust code. The v1 vectors (the encryption) have their own check."""
import base64, hashlib, json, struct, sys
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.exceptions import InvalidSignature

def b64d(s): return base64.urlsafe_b64decode(s + '=' * (-len(s) % 4))
def raw(k): return k.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)

v = json.load(open(sys.argv[1]))
owner = v['owner'].encode()
keys = {}
for c in v['clients']:
    seed = HKDF(algorithm=hashes.SHA256(), length=32, salt=None, info=v['sign_info'].encode()).derive(bytes.fromhex(c['x25519_scalar']))
    sk = ed25519.Ed25519PrivateKey.from_private_bytes(seed)
    assert raw(sk.public_key()).hex() == c['sign_key'], 'signing key'
    keys[c['client_key']] = sk.public_key()
spd = v['signer_proof_data']
assert spd == {'kind': 'pubky.music.vault-signer', 'v': 1, 'client_key': v['clients'][0]['client_key'], 'sign_key': v['clients'][0]['sign_key']}

def signed_data(path, env):
    r = hashlib.sha256()
    for x in env['recipients']:
        r.update(bytes.fromhex(x['client_key']) + b64d(x['epk']) + b64d(x['wrapped_key']))
    c = hashlib.sha256(b64d(env['nonce']) + b64d(env['ciphertext']))
    return (b'pubky.music/vault/v2/blob\0' + owner + b'\0' + path.encode() + b'\0'
            + struct.pack('>Q', env['epoch']) + r.digest() + c.digest())

def verifies(path, env):
    if not env.get('writer'): return False
    try:
        keys[env['writer']['client_key']].verify(b64d(env['writer']['sig']), signed_data(path, env))
        return True
    except InvalidSignature:
        return False

case = v['case']
assert signed_data(v['path'], case['envelope']).hex() == case['signed_data'], 'signed data'
assert verifies(v['path'], case['envelope']), 'the writer\'s signature'
print('ok:', case['name'])
for mf in v['must_fail']:
    assert not verifies(mf['path'], mf['envelope']), mf['name'] + ' verifies'
    print('ok: refused,', mf['name'])
