"""Usage: python3 -I check_vault_v1.py vault-v1.json (needs the `cryptography` package).

Independent check of VAULT.md v1 vectors: X25519, HKDF and ChaCha20-Poly1305 from `cryptography`,
HChaCha20 written out here (RFC draft-irtf-cfrg-xchacha), nothing from the Rust code."""
import base64, json, struct, sys
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

def rotl(v, c): return ((v << c) & 0xffffffff) | (v >> (32 - c))
def qr(s, a, b, c, d):
    s[a]=(s[a]+s[b])&0xffffffff; s[d]=rotl(s[d]^s[a],16)
    s[c]=(s[c]+s[d])&0xffffffff; s[b]=rotl(s[b]^s[c],12)
    s[a]=(s[a]+s[b])&0xffffffff; s[d]=rotl(s[d]^s[a],8)
    s[c]=(s[c]+s[d])&0xffffffff; s[b]=rotl(s[b]^s[c],7)
def hchacha20(key, nonce16):
    s = [0x61707865,0x3320646e,0x79622d32,0x6b206574] + list(struct.unpack('<8I', key)) + list(struct.unpack('<4I', nonce16))
    for _ in range(10):
        qr(s,0,4,8,12); qr(s,1,5,9,13); qr(s,2,6,10,14); qr(s,3,7,11,15)
        qr(s,0,5,10,15); qr(s,1,6,11,12); qr(s,2,7,8,13); qr(s,3,4,9,14)
    return struct.pack('<8I', *(s[0:4]+s[12:16]))
def xchacha_dec(key, nonce24, ct, aad):
    sub = hchacha20(key, nonce24[:16])
    return ChaCha20Poly1305(sub).decrypt(b'\0\0\0\0' + nonce24[16:], ct, aad)
def xchacha_enc(key, nonce24, pt, aad):
    sub = hchacha20(key, nonce24[:16])
    return ChaCha20Poly1305(sub).encrypt(b'\0\0\0\0' + nonce24[16:], pt, aad)
def b64d(s): return base64.urlsafe_b64decode(s + '=' * (-len(s) % 4))
def b64e(b): return base64.urlsafe_b64encode(b).rstrip(b'=').decode()
def pub(sec): return X25519PrivateKey.from_private_bytes(sec).public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)

# draft-irtf-cfrg-xchacha §2.2.1 HChaCha20 test vector
k = bytes(range(32)); n = bytes.fromhex('000000090000004a0000000031415927')
assert hchacha20(k, n).hex() == '82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc'

v = json.load(open(sys.argv[1]))
owner, path = v['owner'].encode(), v['path'].encode()
clients = [(bytes.fromhex(c['secret']), bytes.fromhex(c['client_key'])) for c in v['clients']]
for sec, ck in clients: assert pub(sec) == ck, 'client key'
for case in v['cases']:
    env, epoch = case['envelope'], case['epoch']
    e8 = struct.pack('>Q', epoch)
    dk, nonce = bytes.fromhex(case['data_key']), bytes.fromhex(case['nonce'])
    aad = b'pubky.music/vault/v1/blob\0' + owner + b'\0' + path + b'\0' + e8
    # The content, both ways
    assert xchacha_enc(dk, nonce, v['plaintext'].encode(), aad) == b64d(env['ciphertext']), 'content'
    for eph_hex, r in zip(case['ephemeral_secrets'], env['recipients']):
        eph, ck = bytes.fromhex(eph_hex), bytes.fromhex(r['client_key'])
        epk = pub(eph); assert b64e(epk) == r['epk'], 'epk'
        shared = X25519PrivateKey.from_private_bytes(eph).exchange(X25519PublicKey.from_public_bytes(ck))
        info = b'pubky.music/vault/v1/wrap\0' + epk + ck + owner + b'\0' + path + b'\0' + e8
        wk = HKDF(algorithm=hashes.SHA256(), length=32, salt=None, info=info).derive(shared)
        assert xchacha_enc(wk, bytes(24), dk, b'') == b64d(r['wrapped_key']), 'wrap'
        # And the recipient's side: its secret, the epk
        sec = next(s for s, c in clients if c == ck)
        shared2 = X25519PrivateKey.from_private_bytes(sec).exchange(X25519PublicKey.from_public_bytes(epk))
        assert shared2 == shared
        assert xchacha_dec(wk, bytes(24), b64d(r['wrapped_key']), b'') == dk
    print('ok:', case['name'], f"({len(env['recipients'])} recipients)")
