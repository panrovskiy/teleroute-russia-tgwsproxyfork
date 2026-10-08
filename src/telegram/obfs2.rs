use aes::Aes256;
use sha2::{Digest, Sha256};
use ctr::cipher::{KeyIvInit, StreamCipher};
use rand::{rng, RngCore};
use std::io;

pub type AesCtr = ctr::Ctr128BE<Aes256>;

#[derive(Debug, Clone, Copy)]
pub struct ParsedHeader {
    pub protocol: [u8; 4],
    pub dc: i16,
}

pub struct ServerObfs {
    pub parsed: ParsedHeader,
    pub decrypt: AesCtr,
    pub encrypt: AesCtr,
}

pub struct ClientObfs {
    pub raw_header: [u8; 64],
    pub wire_header: [u8; 64],
    pub encrypt: AesCtr,
    pub decrypt: AesCtr,
}

fn make_cipher(key: &[u8], iv: &[u8]) -> io::Result<AesCtr> {
    AesCtr::new_from_slices(key, iv).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid AES-CTR material"))
}

fn filtered_random_header() -> [u8; 64] {
    loop {
        let mut h = [0u8; 64];
        rng().fill_bytes(&mut h);
        let first = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let second = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        if h[0] == 0xef { continue; }
        if matches!(first, 0x4441_4548 | 0x5453_4f50 | 0x2054_4547 | 0x4954_504f | 0xdddd_dddd | 0xeeee_eeee | 0x0201_0316) { continue; }
        if second == 0 { continue; }
        return h;
    }
}

pub fn parse_secret_server_header(wire: [u8; 64], secret: &[u8; 16]) -> io::Result<ServerObfs> {
    let prekey_iv = &wire[8..56];

    let mut hasher = Sha256::new();
    hasher.update(&wire[8..40]);
    hasher.update(secret);
    let key = hasher.finalize();

    let iv = &prekey_iv[32..48];
    let mut handshake_cipher = make_cipher(&key, iv)?;

    let mut decrypted = wire;
    handshake_cipher.apply_keystream(&mut decrypted);

    let protocol = <[u8; 4]>::try_from(&decrypted[56..60]).unwrap();
    if !matches!(
        protocol,
        *b"\xef\xef\xef\xef" | *b"\xee\xee\xee\xee" | *b"\xdd\xdd\xdd\xdd"
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Telegram MTProto protocol tag",
        ));
    }

    let dc = i16::from_le_bytes([decrypted[60], decrypted[61]]);
    if dc == 0 || dc.unsigned_abs() > 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Telegram DC in MTProto secret handshake",
        ));
    }

    // Incoming client payload starts after the 64-byte handshake.
    let mut decrypt = make_cipher(&key, iv)?;
    let mut consumed = [0u8; 64];
    decrypt.apply_keystream(&mut consumed);

    let mut reversed = prekey_iv.to_vec();
    reversed.reverse();

    let mut enc_hasher = Sha256::new();
    enc_hasher.update(&reversed[..32]);
    enc_hasher.update(secret);
    let enc_key = enc_hasher.finalize();

    let encrypt = make_cipher(&enc_key, &reversed[32..48])?;

    Ok(ServerObfs {
        parsed: ParsedHeader { protocol, dc },
        decrypt,
        encrypt,
    })
}

pub fn parse_server_header(wire: [u8; 64]) -> io::Result<ServerObfs> {
    let key = &wire[8..40];
    let iv = &wire[40..56];
    let mut decrypt = make_cipher(key, iv)?;
    let mut skip = [0u8; 56];
    decrypt.apply_keystream(&mut skip);
    let mut tail = wire[56..64].to_vec();
    decrypt.apply_keystream(&mut tail);
    let protocol = <[u8; 4]>::try_from(&tail[0..4]).unwrap();
    let dc = i16::from_le_bytes([tail[4], tail[5]]);
    let mut rev48 = wire[8..56].to_vec();
    rev48.reverse();
    let encrypt = make_cipher(&rev48[..32], &rev48[32..48])?;
    Ok(ServerObfs { parsed: ParsedHeader { protocol, dc }, decrypt, encrypt })
}

pub fn new_client(protocol: [u8; 4], dc: i16) -> io::Result<ClientObfs> {
    let mut raw = filtered_random_header();
    raw[56..60].copy_from_slice(&protocol);
    raw[60..62].copy_from_slice(&dc.to_le_bytes());

    let mut encrypt = make_cipher(&raw[8..40], &raw[40..56])?;
    let mut prefix = vec![0u8; 56];
    encrypt.apply_keystream(&mut prefix);
    let mut tail = raw[56..64].to_vec();
    encrypt.apply_keystream(&mut tail);
    let mut wire = raw;
    wire[56..64].copy_from_slice(&tail);

    let mut rev = raw[8..56].to_vec();
    rev.reverse();
    let decrypt = make_cipher(&rev[..32], &rev[32..48])?;

    // Client -> Telegram payload follows immediately after the 64-byte header.
    let _ = prefix;
    // Server -> client payload starts at a fresh stream position.
    let mut client_encrypt = make_cipher(&raw[8..40], &raw[40..56])?;
    let mut header_pad = [0u8; 64];
    client_encrypt.apply_keystream(&mut header_pad);

    Ok(ClientObfs { raw_header: raw, wire_header: wire, encrypt: client_encrypt, decrypt })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header_parse() {
        let h = new_client(*b"\xee\xee\xee\xee", 2).unwrap();
        let parsed = parse_server_header(h.wire_header).unwrap();
        assert_eq!(parsed.parsed.protocol, *b"\xee\xee\xee\xee");
        assert_eq!(parsed.parsed.dc, 2);
    }

    #[test]
    fn roundtrip_secret_header_parse() {
        let secret = [0x11u8; 16];
        let mut plain = filtered_random_header();
        plain[56..60].copy_from_slice(b"\xee\xee\xee\xee");
        plain[60..62].copy_from_slice(&(-2i16).to_le_bytes());

        let mut hasher = Sha256::new();
        hasher.update(&plain[8..40]);
        hasher.update(&secret);
        let key = hasher.finalize();

        let mut cipher = make_cipher(&key, &plain[40..56]).unwrap();
        let mut wire = plain;
        cipher.apply_keystream(&mut wire);

        let parsed = parse_secret_server_header(wire, &secret).unwrap();
        assert_eq!(parsed.parsed.protocol, *b"\xee\xee\xee\xee");
        assert_eq!(parsed.parsed.dc, -2);
    }
}
