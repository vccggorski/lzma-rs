use crate::allocator::Allocator;
use crate::decode::util;
use crate::error;
use crate::io_ext::ReadBytesExt;
use byteorder::BigEndian;
use core2::io;

pub struct RangeDecoder<'a, R>
where
    R: 'a + io::BufRead,
{
    pub stream: &'a mut R,
    pub range: u32,
    pub code: u32,
}

impl<'a, R> RangeDecoder<'a, R>
where
    R: io::BufRead,
{
    pub fn new(stream: &'a mut R) -> io::Result<Self> {
        let mut dec = Self {
            stream,
            range: 0xFFFF_FFFF,
            code: 0,
        };
        let _ = dec.stream.read_u8()?;
        dec.code = dec.stream.read_u32::<BigEndian>()?;
        lzma_debug!("0 {{ range: {:08x}, code: {:08x} }}", dec.range, dec.code);
        Ok(dec)
    }

    pub fn from_parts(stream: &'a mut R, range: u32, code: u32) -> Self {
        Self {
            stream,
            range,
            code,
        }
    }

    pub fn set(&mut self, range: u32, code: u32) {
        self.range = range;
        self.code = code;
    }

    pub fn read_into(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        self.stream.read(dst)
    }

    #[inline]
    pub fn is_finished_ok(&mut self) -> io::Result<bool> {
        Ok(self.code == 0 && self.is_eof()?)
    }

    #[inline]
    pub fn is_eof(&mut self) -> io::Result<bool> {
        util::is_eof(self.stream)
    }

    #[inline]
    fn normalize(&mut self) -> io::Result<()> {
        lzma_trace!("  {{ range: {:08x}, code: {:08x} }}", self.range, self.code);
        if self.range < 0x0100_0000 {
            self.range <<= 8;
            self.code = (self.code << 8) ^ (self.stream.read_u8()? as u32);

            lzma_debug!("+ {{ range: {:08x}, code: {:08x} }}", self.range, self.code);
        }
        Ok(())
    }

    #[inline]
    fn get_bit(&mut self) -> error::Result<bool> {
        self.range >>= 1;

        let bit = self.code >= self.range;
        if bit {
            self.code -= self.range
        }

        self.normalize()?;
        Ok(bit)
    }

    pub fn get(&mut self, count: usize) -> error::Result<u32> {
        let mut result = 0u32;
        for _ in 0..count {
            result = (result << 1) ^ (self.get_bit()? as u32)
        }
        Ok(result)
    }

    #[inline]
    pub fn decode_bit(&mut self, prob: &mut u16, update: bool) -> io::Result<bool> {
        let bound: u32 = (self.range >> 11) * (*prob as u32);

        lzma_trace!(
            " bound: {:08x}, prob: {:04x}, bit: {}",
            bound,
            prob,
            (self.code > bound) as u8
        );
        if self.code < bound {
            if update {
                *prob += (0x800_u16 - *prob) >> 5;
            }
            self.range = bound;

            self.normalize()?;
            Ok(false)
        } else {
            if update {
                *prob -= *prob >> 5;
            }
            self.code -= bound;
            self.range -= bound;

            self.normalize()?;
            Ok(true)
        }
    }

    fn parse_bit_tree(
        &mut self,
        num_bits: usize,
        probs: &mut [u16],
        update: bool,
    ) -> io::Result<u32> {
        let mut tmp: u32 = 1;
        for _ in 0..num_bits {
            let bit = self.decode_bit(&mut probs[tmp as usize], update)?;
            tmp = (tmp << 1) ^ (bit as u32);
        }
        Ok(tmp - (1 << num_bits))
    }

    pub fn parse_reverse_bit_tree(
        &mut self,
        num_bits: usize,
        probs: &mut [u16],
        offset: usize,
        update: bool,
    ) -> io::Result<u32> {
        let mut result = 0u32;
        let mut tmp: usize = 1;
        for i in 0..num_bits {
            let bit = self.decode_bit(&mut probs[offset + tmp], update)?;
            tmp = (tmp << 1) ^ (bit as usize);
            result ^= (bit as u32) << i;
        }
        Ok(result)
    }
}

pub trait AbstractBitTree {
    fn num_bits(&self) -> usize;
    fn probs(&mut self) -> &mut [u16];
    fn parse<R: io::BufRead>(
        &mut self,
        rangecoder: &mut RangeDecoder<R>,
        update: bool,
    ) -> io::Result<u32> {
        rangecoder.parse_bit_tree(self.num_bits(), self.probs(), update)
    }

    fn parse_reverse<R: io::BufRead>(
        &mut self,
        rangecoder: &mut RangeDecoder<R>,
        update: bool,
    ) -> io::Result<u32> {
        rangecoder.parse_reverse_bit_tree(self.num_bits(), self.probs(), 0, update)
    }
}

impl<'a> AbstractBitTree for BitTree<'a> {
    fn num_bits(&self) -> usize {
        self.num_bits
    }
    fn probs(&mut self) -> &mut [u16] {
        self.probs
    }
}

// TODO: parametrize by constant and use [u16; 1 << num_bits] as soon as Rust
// supports this
pub struct BitTree<'a> {
    num_bits: usize,
    probs: &'a mut [u16],
}

impl<'a> BitTree<'a> {
    pub fn new<A: Allocator>(mm: &'a A, num_bits: usize) -> Result<Self, A::Error> {
        Ok(Self {
            num_bits,
            probs: mm.allocate(1 << num_bits, || Ok(0x400))?,
        })
    }
}

#[cfg(feature = "std")]
impl AbstractBitTree for StdBitTree {
    fn num_bits(&self) -> usize {
        self.num_bits
    }
    fn probs(&mut self) -> &mut [u16] {
        self.probs.as_mut_slice()
    }
}

// TODO: parametrize by constant and use [u16; 1 << num_bits] as soon as Rust
// supports this
#[cfg(feature = "std")]
#[derive(Clone)]
pub struct StdBitTree {
    num_bits: usize,
    probs: Vec<u16>,
}

#[cfg(feature = "std")]
impl StdBitTree {
    pub fn new(num_bits: usize) -> Self {
        Self {
            num_bits,
            probs: vec![0x400; 1 << num_bits],
        }
    }
}

pub trait AbstractLenDecoder {
    type BitTree: AbstractBitTree;
    fn choice(&mut self) -> &mut u16;
    fn choice2(&mut self) -> &mut u16;
    fn low_coder(&mut self) -> &mut [Self::BitTree];
    fn mid_coder(&mut self) -> &mut [Self::BitTree];
    fn high_coder(&mut self) -> &mut Self::BitTree;

    fn decode<R: io::BufRead>(
        &mut self,
        rangecoder: &mut RangeDecoder<R>,
        pos_state: usize,
        update: bool,
    ) -> io::Result<usize> {
        if !rangecoder.decode_bit(&mut self.choice(), update)? {
            Ok(self.low_coder()[pos_state].parse(rangecoder, update)? as usize)
        } else if !rangecoder.decode_bit(&mut self.choice2(), update)? {
            Ok(self.mid_coder()[pos_state].parse(rangecoder, update)? as usize + 8)
        } else {
            Ok(self.high_coder().parse(rangecoder, update)? as usize + 16)
        }
    }
}

impl<'a> AbstractLenDecoder for LenDecoder<'a> {
    type BitTree = BitTree<'a>;
    fn choice(&mut self) -> &mut u16 {
        &mut self.choice
    }
    fn choice2(&mut self) -> &mut u16 {
        &mut self.choice2
    }
    fn low_coder(&mut self) -> &mut [Self::BitTree] {
        &mut self.low_coder
    }
    fn mid_coder(&mut self) -> &mut [Self::BitTree] {
        &mut self.mid_coder
    }
    fn high_coder(&mut self) -> &mut Self::BitTree {
        &mut self.high_coder
    }
}

pub struct LenDecoder<'a> {
    choice: u16,
    choice2: u16,
    low_coder: &'a mut [BitTree<'a>],
    mid_coder: &'a mut [BitTree<'a>],
    high_coder: BitTree<'a>,
}

impl<'a> LenDecoder<'a> {
    pub fn new<A: Allocator>(mm: &'a A) -> Result<Self, A::Error> {
        Ok(Self {
            choice: 0x400,
            choice2: 0x400,
            low_coder: mm.allocate(16, || BitTree::new(mm, 3))?,
            mid_coder: mm.allocate(16, || BitTree::new(mm, 3))?,
            high_coder: BitTree::new(mm, 8)?,
        })
    }
}

#[cfg(feature = "std")]
impl AbstractLenDecoder for StdLenDecoder {
    type BitTree = StdBitTree;
    fn choice(&mut self) -> &mut u16 {
        &mut self.choice
    }
    fn choice2(&mut self) -> &mut u16 {
        &mut self.choice2
    }
    fn low_coder(&mut self) -> &mut [Self::BitTree] {
        &mut self.low_coder
    }
    fn mid_coder(&mut self) -> &mut [Self::BitTree] {
        &mut self.mid_coder
    }
    fn high_coder(&mut self) -> &mut Self::BitTree {
        &mut self.high_coder
    }
}

#[cfg(feature = "std")]
pub struct StdLenDecoder {
    choice: u16,
    choice2: u16,
    low_coder: Vec<StdBitTree>,
    mid_coder: Vec<StdBitTree>,
    high_coder: StdBitTree,
}

#[cfg(feature = "std")]
impl StdLenDecoder {
    pub fn new() -> Self {
        Self {
            choice: 0x400,
            choice2: 0x400,
            low_coder: vec![StdBitTree::new(3); 16],//mm.allocate(16, || BitTree::new(mm, 3))?,
            mid_coder: vec![StdBitTree::new(3); 16],//mm.allocate(16, || BitTree::new(mm, 3))?,
            high_coder: StdBitTree::new(8),
        }
    }
}
