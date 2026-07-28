/// Minimal RC4 stream cipher implementation.
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for i in 0..256 {
            s[i] = i as u8;
        }
        let mut j: u8 = 0;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Rc4 { s, i: 0, j: 0 }
    }

    /// Discard the first `n` bytes of the keystream (required by MSE: discard 1024).
    pub fn discard(&mut self, n: usize) {
        for _ in 0..n {
            self.next_byte();
        }
    }

    fn next_byte(&mut self) -> u8 {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.s[self.i as usize]);
        self.s.swap(self.i as usize, self.j as usize);
        self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize]
    }

    /// Encrypt/decrypt in place (XOR with keystream).
    pub fn process(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b ^= self.next_byte();
        }
    }

    /// Encrypt/decrypt, returning new buffer.
    pub fn process_bytes(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.process(&mut out);
        out
    }
}
