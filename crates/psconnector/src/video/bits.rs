//! Reading H.264 / H.265 bitstreams: Annex-B start codes and Exp-Golomb fields.

pub(crate) struct Bits {
    pub(crate) data: Vec<u8>,
    pub(crate) at: usize,
}

impl Bits {
    pub(crate) fn from_ebsp(data: &[u8]) -> Self {
        let mut rbsp = Vec::with_capacity(data.len());
        let mut zeros = 0;
        for &byte in data {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Self { data: rbsp, at: 0 }
    }

    pub(crate) fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.at / 8)?;
        let value = u32::from((byte >> (7 - self.at % 8)) & 1);
        self.at += 1;
        Some(value)
    }

    pub(crate) fn bits(&mut self, count: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + if zeros == 0 { 0 } else { self.bits(zeros)? })
    }

    pub(crate) fn se(&mut self) -> Option<i32> {
        let value = self.ue()?;
        Some(if value & 1 == 0 { -((value / 2) as i32) } else { value.div_ceil(2) as i32 })
    }

    pub(crate) fn scaling_list(&mut self, size: usize) -> Option<()> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

pub(crate) fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    for at in from..data.len().saturating_sub(2) {
        if data[at..].starts_with(&[0, 0, 1]) {
            return Some((at, at + 3));
        }
        if data[at..].starts_with(&[0, 0, 0, 1]) {
            return Some((at, at + 4));
        }
    }
    None
}
