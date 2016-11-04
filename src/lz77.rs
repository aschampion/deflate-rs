use std::cmp;

use input_buffer::InputBuffer;
use matching::longest_match;
use lzvalue::LZValue;
use huffman_table;
use chained_hash_table::ChainedHashTable;
use compression_options::HIGH_MAX_HASH_CHECKS;
use output_writer::{OutputWriter, FixedWriter};

const MAX_MATCH: usize = huffman_table::MAX_MATCH as usize;
const MIN_MATCH: usize = huffman_table::MIN_MATCH as usize;

/// A struct that contains the hash table, and keeps track of where we are in the input data
pub struct LZ77State {
    hash_table: ChainedHashTable,
    // The current position in the input slice
    current_start: usize,
    // True if this is the first window
    is_first_window: bool,
    // True if the last block has been output
    is_last_block: bool,
    // How many bytes the last match in the previous window extended into the current one
    overlap: usize,
    // The maximum number of hash entries to search
    max_hash_checks: u16,
}

impl LZ77State {
    fn from_starting_values(b0: u8, b1: u8, max_hash_checks: u16) -> LZ77State {
        LZ77State {
            hash_table: ChainedHashTable::from_starting_values(b0, b1),
            current_start: 0,
            is_first_window: true,
            is_last_block: false,
            overlap: 0,
            max_hash_checks: max_hash_checks,
        }
    }

    /// Creates a new LZ77 state, adding the first to bytes to the hash table
    /// to warm it up
    pub fn new(data: &[u8], max_hash_checks: u16) -> LZ77State {
        LZ77State::from_starting_values(data[0], data[1], max_hash_checks)
    }

    pub fn set_last(&mut self) {
        self.is_last_block = true;
    }

    pub fn is_last_block(&self) -> bool {
        self.is_last_block
    }
}

/// A structure representing either a literal, length or distance value
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum LDPair {
    Literal(u8),
    Length(u16),
    Distance(u16),
}

const DEFAULT_WINDOW_SIZE: usize = 32768;

fn process_chunk<W: OutputWriter>(data: &[u8],
                                  start: usize,
                                  end: usize,
                                  hash_table: &mut ChainedHashTable,
                                  writer: &mut W,
                                  max_hash_checks: u16)
                                  -> usize {
    let end = cmp::min(data.len(), end);
    let current_chunk = &data[start..end];

    let mut insert_it = current_chunk.iter().enumerate();
    let mut hash_it = {
        let hash_start = if end - start > 2 {
            start + 2
        } else {
            data.len()
        };
        (&data[hash_start..]).iter()
    };

    const NO_LENGTH: usize = MIN_MATCH as usize - 1;

    let mut prev_byte = 0u8;
    let mut prev_length = NO_LENGTH;
    let mut prev_distance = 0;
    let mut add = false;
    let mut overlap = 0;

    // Iterate through the slice, adding literals or length/distance pairs
    while let Some((n, &b)) = insert_it.next() {
        if let Some(&hash_byte) = hash_it.next() {
            let position = n + start;
            hash_table.add_hash_value(position, hash_byte);
            // rolling_checksum.update(hash_byte);

            let (match_len, match_dist) =
                longest_match(data, hash_table, position, prev_length, max_hash_checks);

            if prev_length >= match_len && prev_length >= MIN_MATCH as usize {
                // The previous match was better so we add it
                // Casting note: length and distance is already bounded by the longest match
                // function. Usize is just used for convenience
                writer.write_length_distance(prev_length as u16, prev_distance as u16);

                // We add the bytes to the hash table and checksum.
                // Since we've already added two of them, we need to add two less than
                // the length
                let bytes_to_add = prev_length - 2;
                let taker = insert_it.by_ref().take(bytes_to_add);
                let mut hash_taker = hash_it.by_ref().take(bytes_to_add);

                // Advance the iterators and add the bytes we jump over to the hash table and
                // checksum
                for (ipos, _) in taker {
                    if let Some(&i_hash_byte) = hash_taker.next() {
                        // rolling_checksum.update(i_hash_byte);
                        hash_table.add_hash_value(ipos + start, i_hash_byte);
                    }
                }

                if position + prev_length > end {
                    // We need to subtract 1 since the byte at pos is also included
                    overlap = position + prev_length - end - 1;
                };

                add = false;

            } else if add {
                // We found a better match (or there was no previous match)
                // so output the previous byte
                writer.write_literal(prev_byte);
            } else {
                add = true
            }

            prev_length = match_len;
            prev_distance = match_dist;
            prev_byte = b;
        } else {
            if add {
                // We may still have a leftover byte at this point, so we add it here if needed.
                writer.write_literal(prev_byte);
                add = false;
            }
            // We are at the last two bytes we want to add, so there is no point
            // searching for matches here.
            writer.write_literal(b);
        }
    }
    if add {
        // We may still have a leftover byte at this point, so we add it here if needed.
        writer.write_literal(prev_byte);
    }
    overlap
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum LZ77Status {
    NoInput,
    EndBlock,
}

/// Compress a slice
/// Will return err on failure eventually, but for now allways succeeds or panics
pub fn lz77_compress_block<W: OutputWriter>(data: &[u8],
                                            state: &mut LZ77State,
                                            buffer: &mut InputBuffer,
                                            mut writer: &mut W)
                                            -> LZ77Status {
    // Currently we use window size as block length, in the future we might want to allow
    // differently sized blocks
    let window_size = DEFAULT_WINDOW_SIZE;

    // If the next block is very short, we merge it into the current block as the huffman tables
    // for a new block may otherwise waste space.
    //    const MIN_BLOCK_LENGTH: usize = 10000;
    // let next_block_merge = data.len() - state.current_start > window_size &&
    // data.len() - state.current_start - window_size < MIN_BLOCK_LENGTH;
    //

    let mut status = LZ77Status::EndBlock;

    while writer.buffer_length() < (window_size * 2) && status != LZ77Status::NoInput {

        if state.is_first_window {

            //        let first_chunk_end = if next_block_merge {
            // data.len()
            // } else {
            // cmp::min(window_size, data.len())
            // };

            let first_chunk_end = cmp::min(window_size, data.len());

            state.overlap = process_chunk::<W>(buffer.get_buffer(),
                                               0,
                                               first_chunk_end,
                                               &mut state.hash_table,
                                               &mut writer,
                                               state.max_hash_checks);
            // We are at the first block so we don't need to slide the hash table
            state.current_start += first_chunk_end;
            if first_chunk_end >= data.len() {
                state.set_last();
                status = LZ77Status::NoInput;
            } else {
                status = LZ77Status::EndBlock;
            }
            state.is_first_window = false;
            return status;
        } else {
            //        for _ in 0..1 {
            let start = state.current_start;
            let slice = &data[start - window_size..];
            // Where we have to stop iterating to slide the buffer and hash,
            // or stop because we are at the end of the input data.
            let end = cmp::min(window_size * 2, slice.len());
            // Limit the length of the input buffer slice so we don't go off the end
            // and read garbage data when checking match lengths.
            let buffer_end = cmp::min(window_size * 2 + MAX_MATCH, slice.len());

            state.overlap = process_chunk::<W>(&buffer.get_buffer()[..buffer_end],
                                               window_size + state.overlap,
                                               end,
                                               &mut state.hash_table,
                                               &mut writer,
                                               state.max_hash_checks);
            if end >= slice.len() {
                // We stopped before or at the window size, so we are at the end.
                state.set_last();
                status = LZ77Status::NoInput;
            } else {
                // We are not at the end, so slide and continue
                state.current_start += window_size;
                let start = state.current_start;
                // We slide the hash table back to make space for new hash values
                // We only need to remember 32k bytes back (the maximum distance allowed by the
                // deflate spec)
                state.hash_table.slide(window_size);
                let end = cmp::min(start + window_size + MAX_MATCH, data.len());
                //                rolling_checksum.update_from_slice(&data[start + 2..end]);
                // slide_buffer(buffer, &data[start..end]);
                buffer.slide(&data[start..end]);
                status = LZ77Status::EndBlock;
            }

            //            if !next_block_merge {
            // break;
            // }
            // }
        }
    }

    status
}

#[allow(dead_code)]
pub struct TestStruct {
    state: LZ77State,
    buffer: InputBuffer,
    writer: FixedWriter,
}

/// Compress a slice, not storing frequency information
///
/// This is a convenience function for compression with fixed huffman values
/// Only used in tests for now
#[allow(dead_code)]
pub fn lz77_compress(data: &[u8]) -> Option<Vec<LZValue>> {
    let mut test_boxed = Box::new(TestStruct {
        state: LZ77State::new(data, HIGH_MAX_HASH_CHECKS),
        buffer: InputBuffer::new(data).0,
        writer: FixedWriter::new(),
    });
    let mut out = Vec::<LZValue>::with_capacity(data.len() / 3);
    {
        let mut test = test_boxed.as_mut();

        while !test.state.is_last_block {
            lz77_compress_block(data, &mut test.state, &mut test.buffer, &mut test.writer);
            out.extend(test.writer.get_buffer());
            test.writer.clear_buffer();
        }

    }

    Some(out)
}

#[cfg(test)]
mod test {
    use super::*;
    use lzvalue::LZValue;

    fn decompress_lz77(input: &[LZValue]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut last_length = 0;
        for p in input {
            match p.value() {
                LDPair::Literal(l) => output.push(l),
                LDPair::Length(l) => last_length = l,
                LDPair::Distance(d) => {
                    let start = output.len() - d as usize;
                    let mut n = 0;
                    while n < last_length as usize {
                        let b = output[start + n];
                        output.push(b);
                        n += 1;
                    }
                }
            }
        }
        output
    }


    /// Helper function to print the output from the lz77 compression function
    fn print_output(input: &[LZValue]) {
        let mut output = vec![];
        for l in input {
            match l.value() {
                LDPair::Literal(l) => output.push(l),
                LDPair::Length(l) => output.extend(format!("<L {}>\n", l).into_bytes()),
                LDPair::Distance(d) => output.extend(format!("<D {}>\n", d).into_bytes()),
            }
        }

        println!("\"{}\"", String::from_utf8(output).unwrap());
    }

    /// Test that a short string from an example on SO compresses correctly
    #[test]
    fn test_lz77_short() {
        use std::str;

        let test_bytes = String::from("Deflate late").into_bytes();
        let res = lz77_compress(&test_bytes).unwrap();
        // println!("{:?}", res);
        // TODO: Check that compression is correct
        // print_output(&res);
        let decompressed = decompress_lz77(&res);
        let d_str = str::from_utf8(&decompressed).unwrap();
        println!("{}", d_str);
        assert_eq!(test_bytes, decompressed);
        // assert_eq!(res[8],
        // LDPair::LengthDistance {
        // distance: 5,
        // length: 4,
        // });
    }

    fn get_test_file_data(name: &str) -> Vec<u8> {
        use std::fs::File;
        use std::io::Read;
        let mut input = Vec::new();
        let mut f = File::open(name).unwrap();

        f.read_to_end(&mut input).unwrap();
        input
    }

    fn get_test_data() -> Vec<u8> {
        use std::env;
        let path = env::var("TEST_FILE").unwrap_or("tests/pg11.txt".to_string());
        get_test_file_data(&path)
    }


    /// Test that compression is working for a longer file
    #[test]
    fn test_lz77_long() {
        use std::str;
        let input = get_test_data();
        let compressed = lz77_compress(&input).unwrap();
        assert!(compressed.len() < input.len());
        // print_output(&compressed);
        let decompressed = decompress_lz77(&compressed);
        // println!("{}", str::from_utf8(&decompressed).unwrap());
        // This is to check where the compression fails, if it were to
        for (n, (&a, &b)) in input.iter().zip(decompressed.iter()).enumerate() {
            if a != b {
                println!("First difference at {}, input: {}, output: {}", n, a, b);
                break;
            }
        }
        assert_eq!(input.len(), decompressed.len());
        assert!(&decompressed == &input);
    }

    /// Check that lazy matching is working as intended
    #[test]
    fn test_lazy() {
        // We want to match on `badger` rather than `nba` as it is longer
        // let data = b" nba nbadg badger nbadger";
        let data = b"nba badger nbadger";
        let compressed = lz77_compress(data).unwrap();
        let test = compressed[compressed.len() - 2];
        if let LDPair::Length(n) = test.value() {
            assert_eq!(n, 6);
        } else {
            print_output(&compressed);
            panic!();
        }
    }

    fn roundtrip(data: &[u8]) {
        let compressed = super::lz77_compress(&data).unwrap();
        let decompressed = decompress_lz77(&compressed);
        assert!(decompressed == data);
    }

    // Check that data with the exact window size is working properly
    #[test]
    #[allow(unused)]
    fn test_exact_window_size() {
        use chained_hash_table::WINDOW_SIZE;
        use std::io::Write;
        let mut data = vec![0; WINDOW_SIZE];
        roundtrip(&data);
        {
            data.write(&[22; WINDOW_SIZE]);
        }
        roundtrip(&data);
        {
            data.write(&[55; WINDOW_SIZE]);
        }
        roundtrip(&data);
    }

    /// Test that matches at the window border are working correctly
    #[test]
    fn test_lz77_border() {
        use chained_hash_table::WINDOW_SIZE;
        let data = vec![0; WINDOW_SIZE + 50];
        let compressed = super::lz77_compress(&data).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = decompress_lz77(&compressed);
        assert!(decompressed == data);
    }

    #[test]
    fn test_lz77_border_multiple_blocks() {
        use chained_hash_table::WINDOW_SIZE;
        let mut data = vec![0; (WINDOW_SIZE * 2) + 50];
        data.push(1);
        let compressed = super::lz77_compress(&data).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = decompress_lz77(&compressed);
        assert!(decompressed == data);
    }
}
