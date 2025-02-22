use crate::decode::lzbuffer;
use crate::decode::rangecoder;
use crate::decompress::Options;
use crate::decompress::UnpackedSize;
use crate::error;
use crate::io;
use crate::option::GuaranteedOption as Option;
use crate::option::GuaranteedOption::*;
use byteorder::LittleEndian;
use io::ReadBytesExt;

/// Maximum input data that can be processed in one iteration.
/// Libhtp uses the following equation to define the maximum number of bits
/// for the worst case scenario:
///   log2((2^11 / 31) ^ 22) + 26 < 134 + 26 = 160
const MAX_REQUIRED_INPUT: usize = 20;

/// Processing mode for decompression.
///
/// Tells the decompressor if we should expect more data after parsing the
/// current input.
#[derive(Debug, PartialEq)]
enum ProcessingMode {
    /// Streaming mode. Process the input bytes but assume there will be more
    /// chunks of input data to receive in future calls to `process_mode()`.
    Partial,
    /// Synchronous mode. Process the input bytes and confirm end of stream has
    /// been reached. Use this mode if you are processing a fixed buffer of
    /// compressed data, or after using `Mode::Partial` to check for the end
    /// of stream.
    Finish,
}

/// Result of the next iteration of processing.
///
/// Indicates whether processing should continue or is finished.
#[repr(C)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub(crate) enum ProcessingStatus {
    Uninitialized,
    Continue,
    Finished,
}

#[repr(C)]
#[derive(Clone)]
pub struct LzmaParams {
    // most lc significant bits of previous byte are part of the literal context
    pub lc: u32, // 0..8
    pub lp: u32, // 0..4
    // context for literal/match is plaintext offset modulo 2^pb
    pub pb: u32, // 0..4
    pub dict_size: u32,
    pub unpacked_size: Option<u64>,
}

impl LzmaParams {
    pub fn read_header<R>(input: &mut R, options: &Options) -> error::Result<LzmaParams>
    where
        R: io::BufRead,
    {
        // Properties
        let props = input.read_u8().map_err(error::Error::HeaderTooShort)?;

        let mut pb = props as u32;
        if pb >= 225 {
            return Err(error::lzma::LzmaError::InvalidHeader {
                invalid_properties: pb,
            }
            .into());
        }

        let lc: u32 = pb % 9;
        pb /= 9;
        let lp: u32 = pb % 5;
        pb /= 5;

        lzma_info!("Properties {{ lc: {}, lp: {}, pb: {} }}", lc, lp, pb);

        // Dictionary
        let dict_size_provided = input
            .read_u32::<LittleEndian>()
            .map_err(error::Error::HeaderTooShort)?;
        let dict_size = if dict_size_provided < 0x1000 {
            0x1000
        } else {
            dict_size_provided
        };

        lzma_info!("Dict size: {}", dict_size);

        // Unpacked size
        let unpacked_size: Option<u64> = match options.unpacked_size {
            UnpackedSize::ReadFromHeader => {
                let unpacked_size_provided = input
                    .read_u64::<LittleEndian>()
                    .map_err(error::Error::HeaderTooShort)?;
                let marker_mandatory: bool = unpacked_size_provided == 0xFFFF_FFFF_FFFF_FFFF;
                if marker_mandatory {
                    None
                } else {
                    Some(unpacked_size_provided)
                }
            }
            UnpackedSize::ReadHeaderButUseProvided(x) => {
                input
                    .read_u64::<LittleEndian>()
                    .map_err(error::Error::HeaderTooShort)?;
                x.into()
            }
            UnpackedSize::UseProvided(x) => x,
        };

        lzma_info!("Unpacked size: {:?}", unpacked_size);

        let params = LzmaParams {
            lc,
            lp,
            pb,
            dict_size,
            unpacked_size,
        };

        Ok(params)
    }
}

pub struct DecoderState<LZB, const PROBS_MEM_LIMIT: usize>
where
    LZB: lzbuffer::LzBuffer,
{
    processing_status: ProcessingStatus,
    // Buffer input data here if we need more for decompression. Up to
    // MAX_REQUIRED_INPUT bytes can be consumed during one iteration.
    pub params: Option<LzmaParams>,
    partial_input_buf: io::Cursor<[u8; MAX_REQUIRED_INPUT]>,
    pub output: LZB,
    literal_probs: [[u16; 0x300]; PROBS_MEM_LIMIT],
    pos_slot_decoder: [rangecoder::BitTree<64>; 4],
    align_decoder: rangecoder::BitTree<16>,
    pos_decoders: [u16; 115],
    is_match: [u16; 192], // true = LZ, false = literal
    is_rep: [u16; 12],
    is_rep_g0: [u16; 12],
    is_rep_g1: [u16; 12],
    is_rep_g2: [u16; 12],
    is_rep_0long: [u16; 192],
    state: usize,
    rep: [usize; 4],
    len_decoder: rangecoder::LenDecoder,
    rep_len_decoder: rangecoder::LenDecoder,
}

impl<const DICT_MEM_LIMIT: usize, const PROBS_MEM_LIMIT: usize>
    DecoderState<lzbuffer::LzCircularBuffer<DICT_MEM_LIMIT>, PROBS_MEM_LIMIT>
{
    pub const fn new() -> Self {
        Self {
            processing_status: ProcessingStatus::Uninitialized,
            output: lzbuffer::LzCircularBuffer::new(),
            partial_input_buf: io::Cursor::new([0; MAX_REQUIRED_INPUT]),
            params: None,
            literal_probs: [[0; 0x300]; PROBS_MEM_LIMIT],
            pos_slot_decoder: [rangecoder::BitTree::new(); 4],
            align_decoder: rangecoder::BitTree::new(),
            pos_decoders: [0; 115],
            is_match: [0; 192],
            is_rep: [0; 12],
            is_rep_g0: [0; 12],
            is_rep_g1: [0; 12],
            is_rep_g2: [0; 12],
            is_rep_0long: [0; 192],
            state: 0,
            rep: [0; 4],
            len_decoder: rangecoder::LenDecoder::new(),
            rep_len_decoder: rangecoder::LenDecoder::new(),
        }
    }
}

impl<LZB, const PROBS_MEM_LIMIT: usize> DecoderState<LZB, PROBS_MEM_LIMIT>
where
    LZB: lzbuffer::LzBuffer,
{
    #[allow(dead_code)]
    pub(crate) fn get_processing_status(&self) -> ProcessingStatus {
        self.processing_status
    }

    pub fn set_params(&mut self, params: LzmaParams) -> error::Result<()> {
        if let ProcessingStatus::Uninitialized = self.processing_status {
            panic!("DecoderState is uninitialized; call `DecoderState::reset` first");
        }
        if (1 << (params.lc + params.lp)) > PROBS_MEM_LIMIT {
            return Err(error::Error::ProbabilitiesBufferTooSmall {
                needed: 1 << (params.lc + params.lp),
                available: PROBS_MEM_LIMIT,
            });
        }
        self.output.set_dict_size(params.dict_size as usize)?;
        self.params = Some(params);
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn reset(&mut self) {
        self.processing_status = ProcessingStatus::Continue;
        self.output.reset();
        self.partial_input_buf = io::Cursor::new([0; MAX_REQUIRED_INPUT]);
        self.params = None;
        self.literal_probs
            .iter_mut()
            .for_each(|v| v.iter_mut().for_each(|v| *v = 0x400));
        self.pos_slot_decoder.iter_mut().for_each(|v| v.reset());
        self.align_decoder.reset();
        self.pos_decoders.iter_mut().for_each(|v| *v = 0x400);
        self.is_match.iter_mut().for_each(|v| *v = 0x400);
        self.is_rep.iter_mut().for_each(|v| *v = 0x400);
        self.is_rep_g0.iter_mut().for_each(|v| *v = 0x400);
        self.is_rep_g1.iter_mut().for_each(|v| *v = 0x400);
        self.is_rep_g2.iter_mut().for_each(|v| *v = 0x400);
        self.is_rep_0long.iter_mut().for_each(|v| *v = 0x400);
        self.state = 0;
        self.rep = [0; 4];
        self.len_decoder.reset();
        self.rep_len_decoder.reset();
    }

    pub fn process<'a, R: io::BufRead>(
        &mut self,
        output: &mut dyn io::Write,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
    ) -> error::Result<()> {
        self.process_mode(output, rangecoder, ProcessingMode::Finish)
    }

    #[cfg(feature = "stream")]
    pub fn process_stream<'a, R: io::BufRead>(
        &mut self,
        output: &mut dyn io::Write,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
    ) -> error::Result<()> {
        self.process_mode(output, rangecoder, ProcessingMode::Partial)
    }

    /// Process the next iteration of the loop.
    ///
    /// If the update flag is true, the decoder's state will be updated.
    ///
    /// Returns `ProcessingStatus` to determine whether one should continue
    /// processing the loop.
    fn process_next_inner<'a, R: io::BufRead>(
        &mut self,
        output: &mut dyn io::Write,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
        update: bool,
    ) -> error::Result<ProcessingStatus> {
        let params = match &self.params {
            Some(v) => v.clone(),
            None => panic!(
                "DecoderState::params is not initialized; call `DecoderState::set_params` first"
            ),
        };
        let pos_state = self.output.len() & ((1 << params.pb) - 1);

        // Literal
        if !rangecoder.decode_bit(
            // TODO: assumes pb = 2 ??
            &mut self.is_match[(self.state << 4) + pos_state],
            update,
        )? {
            let byte: u8 = self.decode_literal(rangecoder, update)?;

            if update {
                lzma_debug!("Literal: {}", byte);
                self.output.append_literal(output, byte)?;

                self.state = if self.state < 4 {
                    0
                } else if self.state < 10 {
                    self.state - 3
                } else {
                    self.state - 6
                };
            }
            return Ok(ProcessingStatus::Continue);
        }

        // LZ
        let mut len: usize;
        // Distance is repeated from LRU
        if rangecoder.decode_bit(&mut self.is_rep[self.state], update)? {
            // dist = rep[0]
            if !rangecoder.decode_bit(&mut self.is_rep_g0[self.state], update)? {
                // len = 1
                if !rangecoder.decode_bit(
                    &mut self.is_rep_0long[(self.state << 4) + pos_state],
                    update,
                )? {
                    // update state (short rep)
                    if update {
                        self.state = if self.state < 7 { 9 } else { 11 };
                        let dist = self.rep[0] + 1;
                        self.output.append_lz(output, 1, dist)?;
                    }
                    return Ok(ProcessingStatus::Continue);
                }
            // dist = rep[i]
            } else {
                let idx: usize;
                if !rangecoder.decode_bit(&mut self.is_rep_g1[self.state], update)? {
                    idx = 1;
                } else if !rangecoder.decode_bit(&mut self.is_rep_g2[self.state], update)? {
                    idx = 2;
                } else {
                    idx = 3;
                }
                if update {
                    // Update LRU
                    let dist = self.rep[idx];
                    for i in (0..idx).rev() {
                        self.rep[i + 1] = self.rep[i];
                    }
                    self.rep[0] = dist
                }
            }

            len = self.rep_len_decoder.decode(rangecoder, pos_state, update)?;

            if update {
                // update state (rep)
                self.state = if self.state < 7 { 8 } else { 11 };
            }
        // New distance
        } else {
            if update {
                // Update LRU
                self.rep[3] = self.rep[2];
                self.rep[2] = self.rep[1];
                self.rep[1] = self.rep[0];
            }

            len = self.len_decoder.decode(rangecoder, pos_state, update)?;

            if update {
                // update state (match)
                self.state = if self.state < 7 { 7 } else { 10 };
            }

            let rep_0 = self.decode_distance(rangecoder, len, update)?;

            if update {
                self.rep[0] = rep_0;
                if self.rep[0] == 0xFFFF_FFFF {
                    if rangecoder.is_finished_ok()? {
                        self.processing_status = ProcessingStatus::Finished;
                        return Ok(ProcessingStatus::Finished);
                    }
                    return Err(error::lzma::LzmaError::EosFoundButMoreBytesAvailable.into());
                }
            }
        }

        if update {
            len += 2;

            let dist = self.rep[0] + 1;
            self.output.append_lz(output, len, dist)?;
        }

        Ok(ProcessingStatus::Continue)
    }

    fn process_next<'a, R: io::BufRead>(
        &mut self,
        output: &mut dyn io::Write,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
    ) -> error::Result<ProcessingStatus> {
        self.process_next_inner(output, rangecoder, true)
    }

    /// Try to process the next iteration of the loop.
    ///
    /// This will check to see if there is enough data to consume and advance
    /// the decompressor. Needed in streaming mode to avoid corrupting the
    /// state while processing incomplete chunks of data.
    fn try_process_next(
        &mut self,
        output: &mut dyn io::Write,
        buf: &[u8],
        range: u32,
        code: u32,
    ) -> error::Result<()> {
        let mut temp = io::Cursor::new(buf);
        let mut rangecoder = rangecoder::RangeDecoder::from_parts(&mut temp, range, code);
        let _ = self.process_next_inner(output, &mut rangecoder, false)?;
        Ok(())
    }

    /// Utility function to read data into the partial input buffer.
    fn read_partial_input_buf<'a, R: io::BufRead>(
        &mut self,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
    ) -> error::Result<()> {
        // Fill as much of the tmp buffer as possible
        let start = self.partial_input_buf.position() as usize;
        let bytes_read =
            rangecoder.read_into(&mut self.partial_input_buf.get_mut()[start..])? as u64;
        self.partial_input_buf
            .set_position(self.partial_input_buf.position() + bytes_read);
        Ok(())
    }

    fn process_mode<'a, R: io::BufRead>(
        &mut self,
        output: &mut dyn io::Write,
        mut rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
        mode: ProcessingMode,
    ) -> error::Result<()> {
        if let ProcessingStatus::Uninitialized = self.processing_status {
            panic!("DecoderState is uninitialized; call `DecoderState::reset` first");
        }
        let params = match &self.params {
            Some(v) => v.clone(),
            None => panic!(
                "DecoderState::params is not initialized; call `DecoderState::set_params` first"
            ),
        };
        loop {
            if let Some(unpacked_size) = params.unpacked_size {
                if self.output.len() as u64 >= unpacked_size {
                    break;
                }
            } else if match mode {
                ProcessingMode::Partial => {
                    rangecoder.is_eof()? && self.partial_input_buf.position() as usize == 0
                }
                ProcessingMode::Finish => {
                    rangecoder.is_finished_ok()? && self.partial_input_buf.position() as usize == 0
                }
            } {
                break;
            }

            if self.partial_input_buf.position() as usize > 0 {
                self.read_partial_input_buf(rangecoder)?;
                let tmp = *self.partial_input_buf.get_ref();

                // Check if we need more data to advance the decompressor
                if mode == ProcessingMode::Partial
                    && (self.partial_input_buf.position() as usize) < MAX_REQUIRED_INPUT
                    && self
                        .try_process_next(
                            output,
                            &tmp[..self.partial_input_buf.position() as usize],
                            rangecoder.range,
                            rangecoder.code,
                        )
                        .is_err()
                {
                    return Ok(());
                }

                // Run the decompressor on the tmp buffer
                let mut tmp_reader =
                    io::Cursor::new(&tmp[..self.partial_input_buf.position() as usize]);
                let mut tmp_rangecoder = rangecoder::RangeDecoder::from_parts(
                    &mut tmp_reader,
                    rangecoder.range,
                    rangecoder.code,
                );
                let res = self.process_next(output, &mut tmp_rangecoder)?;

                // Update the actual rangecoder
                rangecoder.set(tmp_rangecoder.range, tmp_rangecoder.code);

                // Update tmp buffer
                let end = self.partial_input_buf.position();
                let new_len = end - tmp_reader.position();
                self.partial_input_buf.get_mut()[..new_len as usize]
                    .copy_from_slice(&tmp[tmp_reader.position() as usize..end as usize]);
                self.partial_input_buf.set_position(new_len);

                if res == ProcessingStatus::Finished {
                    break;
                };
            } else {
                let buf: &[u8] = rangecoder.stream.fill_buf()?;
                if mode == ProcessingMode::Partial
                    && buf.len() < MAX_REQUIRED_INPUT
                    && self
                        .try_process_next(output, buf, rangecoder.range, rangecoder.code)
                        .is_err()
                {
                    return self.read_partial_input_buf(rangecoder);
                }

                if self.process_next(output, &mut rangecoder)? == ProcessingStatus::Finished {
                    break;
                };
            }
        }

        if let Some(unpacked_size) = params.unpacked_size {
            if mode == ProcessingMode::Finish && unpacked_size != self.output.len() as u64 {
                return Err(
                    error::lzma::LzmaError::ProcessedDataDoesNotMatchUnpackedSize {
                        unpacked_size,
                        decompressed_data: self.output.len(),
                    }
                    .into(),
                );
            }
        }

        Ok(())
    }

    fn decode_literal<'a, R: io::BufRead>(
        &mut self,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
        update: bool,
    ) -> error::Result<u8> {
        let params = match &self.params {
            Some(v) => v.clone(),
            None => panic!(
                "DecoderState::params is not initialized; call `DecoderState::set_params` first"
            ),
        };
        let def_prev_byte = 0u8;
        let prev_byte = self.output.last_or(def_prev_byte) as usize;

        let mut result: usize = 1;
        let lit_state = ((self.output.len() & ((1 << params.lp) - 1)) << params.lc)
            + (prev_byte >> (8 - params.lc));
        let probs = &mut self.literal_probs[lit_state];

        if self.state >= 7 {
            let mut match_byte = self.output.last_n(self.rep[0] + 1)? as usize;

            while result < 0x100 {
                let match_bit = (match_byte >> 7) & 1;
                match_byte <<= 1;
                let bit = rangecoder
                    .decode_bit(&mut probs[((1 + match_bit) << 8) + result], update)?
                    as usize;
                result = (result << 1) ^ bit;
                if match_bit != bit {
                    break;
                }
            }
        }

        while result < 0x100 {
            result = (result << 1) ^ (rangecoder.decode_bit(&mut probs[result], update)? as usize);
        }

        Ok((result - 0x100) as u8)
    }

    fn decode_distance<'a, R: io::BufRead>(
        &mut self,
        rangecoder: &mut rangecoder::RangeDecoder<'a, R>,
        length: usize,
        update: bool,
    ) -> error::Result<usize> {
        let len_state = if length > 3 { 3 } else { length };

        let pos_slot = self.pos_slot_decoder[len_state].parse(rangecoder, update)? as usize;
        if pos_slot < 4 {
            return Ok(pos_slot);
        }

        let num_direct_bits = (pos_slot >> 1) - 1;
        let mut result = (2 ^ (pos_slot & 1)) << num_direct_bits;

        if pos_slot < 14 {
            result += rangecoder.parse_reverse_bit_tree(
                num_direct_bits,
                &mut self.pos_decoders,
                result - pos_slot,
                update,
            )? as usize;
        } else {
            result += (rangecoder.get(num_direct_bits - 4)? as usize) << 4;
            result += self.align_decoder.parse_reverse(rangecoder, update)? as usize;
        }

        Ok(result)
    }
}
