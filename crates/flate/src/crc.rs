/// CRC-32 (IEEE 802.3, reflected polynomial 0xEDB88320), as used by ZIP,
/// gzip and PNG chunks.
#[derive(Debug, Clone)]
pub struct Crc32(u32);

const POLY: u32 = 0xEDB8_8320;

/// One entry per byte value, computed at compile time.
const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

impl Crc32 {
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}
