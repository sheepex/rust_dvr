use aes::cipher::{BlockDecrypt, KeyInit};

const MAX_NAL: usize = 16 * 1024 * 1024;

/// Reassembles H.265 RTP NALs and decrypts the Hikvision-marked payload.
pub struct H265Rtp {
    cipher: Option<aes::Aes128>,
    fragment: Option<(Vec<u8>, bool, u16)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    #[test]
    fn decrypts_only_hik_marked_h265_payloads() {
        let mut receiver = H265Rtp::new("abcdef");
        let mut raw = [0u8; 16];
        raw[0] = 0x10;
        raw[12..14].copy_from_slice(&[0x40, 0x00]);
        let mut original = vec![0x42, 0x01];
        original.extend(0u8..32);
        original.extend_from_slice(&[1, 2, 3]);
        let mut encrypted = original.clone();
        let mut key = [0u8; 16];
        key[..6].copy_from_slice(b"abcdef");
        let cipher = aes::Aes128::new_from_slice(&key).unwrap();
        for block in encrypted[2..34].chunks_exact_mut(16) {
            cipher.encrypt_block(block.into());
        }
        assert_eq!(
            receiver.push(&raw, &encrypted, 1, 0).unwrap(),
            vec![original]
        );
        raw[0] = 0;
        assert_eq!(
            receiver.push(&raw, &encrypted, 2, 0).unwrap(),
            vec![encrypted]
        );
    }

    #[test]
    fn fragments_are_reassembled_and_dropped_on_loss() {
        let mut receiver = H265Rtp::new("abc");
        let raw = [0u8; 12];
        let start = [98, 1, 0x80 | 19, 5, 6];
        let end = [98, 1, 0x40 | 19, 7, 8];
        assert!(receiver.push(&raw, &start, 1, 0).unwrap().is_empty());
        assert_eq!(
            receiver.push(&raw, &end, 2, 0).unwrap(),
            vec![vec![38, 1, 5, 6, 7, 8]]
        );
        assert!(receiver.push(&raw, &start, 3, 0).unwrap().is_empty());
        assert!(receiver.push(&raw, &end, 5, 1).unwrap().is_empty());
    }

    #[test]
    fn invalid_key_does_not_block_unencrypted_stream() {
        let mut receiver = H265Rtp::new("");
        let mut raw = [0u8; 16];
        assert!(receiver.push(&raw, &[0x42, 1, 0], 1, 0).is_ok());
        raw[0] = 0x10;
        raw[12..14].copy_from_slice(&[0x40, 0x00]);
        assert!(receiver.push(&raw, &[0x42, 1, 0], 2, 0).is_err());
    }
}

impl H265Rtp {
    pub fn new(code: &str) -> Self {
        let mut key = [0u8; 16];
        let cipher = if !code.is_empty() && code.is_ascii() && code.len() <= 16 {
            key[..code.len()].copy_from_slice(code.as_bytes());
            aes::Aes128::new_from_slice(&key).ok()
        } else {
            None
        };
        Self {
            cipher,
            fragment: None,
        }
    }

    fn decrypt(&self, mut nal: Vec<u8>, encrypted: bool) -> Result<Vec<u8>, &'static str> {
        let cipher = if encrypted {
            self.cipher
                .as_ref()
                .ok_or("Código de verificação Hikvision inválido ou indisponível.")?
        } else {
            return Ok(nal);
        };
        if encrypted && nal.len() >= 18 {
            let end = 2 + ((nal.len() - 2) / 16 * 16).min(4096);
            for block in nal[2..end].chunks_exact_mut(16) {
                cipher.decrypt_block(block.into());
            }
        }
        Ok(nal)
    }

    pub fn push(
        &mut self,
        raw: &[u8],
        payload: &[u8],
        seq: u16,
        loss: u16,
    ) -> Result<Vec<Vec<u8>>, &'static str> {
        if loss != 0 {
            self.fragment = None;
        }
        if payload.len() < 2 || raw.len() < 12 {
            return Ok(vec![]);
        }
        let offset = 12 + usize::from(raw[0] & 15) * 4;
        let encrypted = raw[0] & 0x10 != 0 && raw.get(offset..offset + 2) == Some(&[0x40, 0x00]);
        let kind = (payload[0] >> 1) & 0x3f;
        match kind {
            0..=47 => Ok(vec![self.decrypt(payload.to_vec(), encrypted)?]),
            48 => {
                let mut result = Vec::new();
                let mut pos = 2;
                while pos + 2 <= payload.len() {
                    let len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
                    pos += 2;
                    if len < 2 || pos + len > payload.len() {
                        return Ok(vec![]);
                    }
                    result.push(payload[pos..pos + len].to_vec());
                    pos += len;
                }
                Ok(if pos == payload.len() { result } else { vec![] })
            }
            49 if payload.len() >= 3 => {
                let header = payload[2];
                if header & 0x80 != 0 {
                    let mut nal = vec![(payload[0] & 0x81) | ((header & 0x3f) << 1), payload[1]];
                    nal.extend_from_slice(&payload[3..]);
                    self.fragment = Some((nal, encrypted, seq));
                } else if let Some((data, _, previous)) = self.fragment.as_mut() {
                    if seq != previous.wrapping_add(1) || data.len() + payload.len() - 3 > MAX_NAL {
                        self.fragment = None;
                        return Ok(vec![]);
                    }
                    data.extend_from_slice(&payload[3..]);
                    *previous = seq;
                }
                if header & 0x40 != 0 {
                    if let Some((data, encrypted, _)) = self.fragment.take() {
                        return Ok(vec![self.decrypt(data, encrypted)?]);
                    }
                }
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }
}
