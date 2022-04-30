use anyhow::{bail, Result};
use indicatif::ProgressBar;
use std::io::Read;
use std::io::{stdout, BufRead, BufReader, BufWriter, Write};
use std::{fs::File, path::Path};

use hevc_parser::hevc::NALUnit;
use hevc_parser::hevc::{NAL_SEI_PREFIX, NAL_UNSPEC62, NAL_UNSPEC63};
use hevc_parser::HevcParser;

use super::{convert_encoded_from_opts, is_st2094_40_sei, CliOptions, IoFormat, OUT_NAL_HEADER};

pub struct DoviReader {
    options: CliOptions,
    rpu_nals: Vec<RpuNal>,

    previous_rpu_index: u64,
}

pub struct DoviWriter {
    bl_writer: Option<BufWriter<File>>,
    el_writer: Option<BufWriter<File>>,
    rpu_writer: Option<BufWriter<File>>,
    sl_writer: Option<BufWriter<File>>,
}

#[derive(Debug)]
pub struct RpuNal {
    decoded_index: usize,
    presentation_number: usize,
    data: Vec<u8>,
}

impl DoviWriter {
    pub fn new(
        bl_out: Option<&Path>,
        el_out: Option<&Path>,
        rpu_out: Option<&Path>,
        single_layer_out: Option<&Path>,
    ) -> DoviWriter {
        let chunk_size = 100_000;
        let bl_writer = bl_out.map(|bl_out| {
            BufWriter::with_capacity(
                chunk_size,
                File::create(bl_out).expect("Can't create file for BL"),
            )
        });

        let el_writer = el_out.map(|el_out| {
            BufWriter::with_capacity(
                chunk_size,
                File::create(el_out).expect("Can't create file for EL"),
            )
        });

        let rpu_writer = rpu_out.map(|rpu_out| {
            BufWriter::with_capacity(
                chunk_size,
                File::create(rpu_out).expect("Can't create file for RPU"),
            )
        });

        let sl_writer = single_layer_out.map(|single_layer_out| {
            BufWriter::with_capacity(
                chunk_size,
                File::create(single_layer_out).expect("Can't create file for SL output"),
            )
        });

        DoviWriter {
            bl_writer,
            el_writer,
            rpu_writer,
            sl_writer,
        }
    }
}

impl DoviReader {
    pub fn new(options: CliOptions) -> DoviReader {
        DoviReader {
            options,
            rpu_nals: Vec::new(),
            previous_rpu_index: 0,
        }
    }

    pub fn read_write_from_io(
        &mut self,
        format: &IoFormat,
        input: &Path,
        pb: Option<&ProgressBar>,
        dovi_writer: &mut DoviWriter,
    ) -> Result<()> {
        //BufReader & BufWriter
        let stdin = std::io::stdin();
        let mut reader = Box::new(stdin.lock()) as Box<dyn BufRead>;

        if let IoFormat::Raw = format {
            let file = File::open(input)?;
            reader = Box::new(BufReader::with_capacity(100_000, file));
        }

        let chunk_size = 100_000;

        let mut main_buf = vec![0; 100_000];
        let mut sec_buf = vec![0; 50_000];

        let mut chunk = Vec::with_capacity(chunk_size);
        let mut end: Vec<u8> = Vec::with_capacity(100_000);

        let mut consumed = 0;

        let mut parser = HevcParser::default();

        let mut offsets = Vec::with_capacity(2048);
        let parse_nals = dovi_writer.rpu_writer.is_some();

        while let Ok(n) = reader.read(&mut main_buf) {
            let mut read_bytes = n;
            if read_bytes == 0 && end.is_empty() && chunk.is_empty() {
                break;
            }

            if *format == IoFormat::RawStdin {
                chunk.extend_from_slice(&main_buf[..read_bytes]);

                loop {
                    let num = reader.read(&mut sec_buf)?;

                    if num > 0 {
                        read_bytes += num;

                        chunk.extend_from_slice(&sec_buf[..num]);

                        if read_bytes >= chunk_size {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            } else if read_bytes < chunk_size {
                chunk.extend_from_slice(&main_buf[..read_bytes]);
            } else {
                chunk.extend_from_slice(&main_buf);
            }

            parser.get_offsets(&chunk, &mut offsets);

            if offsets.is_empty() {
                continue;
            }

            let last = if read_bytes < chunk_size {
                *offsets.last().unwrap()
            } else {
                let last = offsets.pop().unwrap();

                end.clear();
                end.extend_from_slice(&chunk[last..]);

                last
            };

            let nals: Vec<NALUnit> = parser.split_nals(&chunk, &offsets, last, parse_nals)?;
            self.write_nals(&chunk, dovi_writer, &nals)?;

            chunk.clear();

            if !end.is_empty() {
                chunk.extend_from_slice(&end);
                end.clear();
            }

            consumed += read_bytes;

            if consumed >= 100_000_000 {
                if let Some(pb) = pb {
                    pb.inc(1);
                    consumed = 0;
                }
            }
        }

        if let Some(pb) = pb {
            pb.finish_and_clear();
        }

        parser.finish();

        self.flush_writer(&parser, dovi_writer)
    }

    pub fn write_nals(
        &mut self,
        chunk: &[u8],
        dovi_writer: &mut DoviWriter,
        nals: &[NALUnit],
    ) -> Result<()> {
        for nal in nals {
            if self.options.drop_hdr10plus
                && nal.nal_type == NAL_SEI_PREFIX
                && is_st2094_40_sei(&chunk[nal.start..nal.end])?
            {
                continue;
            }

            // Skip duplicate NALUs if they are after a first RPU for the frame
            // Note: Only useful when parsing the NALUs (RPU extraction)
            if self.previous_rpu_index > 0
                && nal.nal_type == NAL_UNSPEC62
                && nal.decoded_frame_index == self.previous_rpu_index
            {
                println!(
                    "Warning: Unexpected RPU NALU found for frame {}. Discarding.",
                    self.previous_rpu_index
                );

                continue;
            }

            if let Some(ref mut sl_writer) = dovi_writer.sl_writer {
                if nal.nal_type == NAL_UNSPEC63 && self.options.discard_el {
                    continue;
                }

                sl_writer.write_all(OUT_NAL_HEADER)?;

                if nal.nal_type == NAL_UNSPEC62 {
                    if let Some(_mode) = self.options.mode {
                        let modified_data =
                            convert_encoded_from_opts(&self.options, &chunk[nal.start..nal.end])?;

                        sl_writer.write_all(&modified_data)?;

                        continue;
                    }
                }

                sl_writer.write_all(&chunk[nal.start..nal.end])?;

                continue;
            }

            match nal.nal_type {
                NAL_UNSPEC63 => {
                    if let Some(ref mut el_writer) = dovi_writer.el_writer {
                        el_writer.write_all(OUT_NAL_HEADER)?;
                        el_writer.write_all(&chunk[nal.start + 2..nal.end])?;
                    }
                }
                NAL_UNSPEC62 => {
                    self.previous_rpu_index = nal.decoded_frame_index;

                    if let Some(ref mut el_writer) = dovi_writer.el_writer {
                        el_writer.write_all(OUT_NAL_HEADER)?;
                    }

                    let rpu_data = &chunk[nal.start..nal.end];

                    // No mode: Copy
                    // Mode 0: Parse, untouched
                    // Mode 1: to MEL
                    // Mode 2: to 8.1
                    // Mode 3: 5 to 8.1
                    if let Some(_mode) = self.options.mode {
                        let modified_data = convert_encoded_from_opts(&self.options, rpu_data)?;

                        if let Some(ref mut _rpu_writer) = dovi_writer.rpu_writer {
                            // RPU for x265, remove 0x7C01
                            self.rpu_nals.push(RpuNal {
                                decoded_index: self.rpu_nals.len(),
                                presentation_number: 0,
                                data: modified_data[2..].to_owned(),
                            });
                        } else if let Some(ref mut el_writer) = dovi_writer.el_writer {
                            el_writer.write_all(&modified_data)?;
                        }
                    } else if let Some(ref mut _rpu_writer) = dovi_writer.rpu_writer {
                        // RPU for x265, remove 0x7C01
                        self.rpu_nals.push(RpuNal {
                            decoded_index: self.rpu_nals.len(),
                            presentation_number: 0,
                            data: rpu_data[2..].to_vec(),
                        });
                    } else if let Some(ref mut el_writer) = dovi_writer.el_writer {
                        el_writer.write_all(rpu_data)?;
                    }
                }
                _ => {
                    if let Some(ref mut bl_writer) = dovi_writer.bl_writer {
                        bl_writer.write_all(OUT_NAL_HEADER)?;
                        bl_writer.write_all(&chunk[nal.start..nal.end])?;
                    }
                }
            }
        }

        Ok(())
    }

    fn flush_writer(&mut self, parser: &HevcParser, dovi_writer: &mut DoviWriter) -> Result<()> {
        if let Some(ref mut bl_writer) = dovi_writer.bl_writer {
            bl_writer.flush()?;
        }

        if let Some(ref mut el_writer) = dovi_writer.el_writer {
            el_writer.flush()?;
        }

        // Reorder RPUs to display output order
        if let Some(ref mut rpu_writer) = dovi_writer.rpu_writer {
            let frames = parser.ordered_frames();

            if frames.is_empty() {
                bail!("No frames parsed!");
            }

            print!("Reordering metadata... ");
            stdout().flush().ok();

            // Sort by matching frame POC
            self.rpu_nals.sort_by_cached_key(|rpu| {
                let matching_index = frames
                    .iter()
                    .position(|f| rpu.decoded_index == f.decoded_number as usize);

                if let Some(i) = matching_index {
                    frames[i].presentation_number
                } else {
                    panic!(
                        "Missing frame/slices for metadata! Decoded index {}",
                        rpu.decoded_index
                    );
                }
            });

            // Set presentation number to new index
            self.rpu_nals
                .iter_mut()
                .enumerate()
                .for_each(|(idx, rpu)| rpu.presentation_number = idx);

            println!("Done.");

            // Write data to file
            for rpu in self.rpu_nals.iter_mut() {
                rpu_writer.write_all(OUT_NAL_HEADER)?;
                rpu_writer.write_all(&rpu.data)?;
            }

            rpu_writer.flush()?;
        }

        Ok(())
    }
}