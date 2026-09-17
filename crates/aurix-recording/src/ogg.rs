use std::io::{self, Write};

const OGG_CAPTURE: &[u8; 4] = b"OggS";

/// Compute Ogg CRC-32 (polynomial 0x04C11DB7, direct, no final XOR).
fn ogg_crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut r = i << 24;
            for _ in 0..8 {
                r = if r & 0x80000000 != 0 {
                    (r << 1) ^ 0x04C11DB7
                } else {
                    r << 1
                };
            }
            t[i as usize] = r;
        }
        t
    });
    let mut crc = 0u32;
    for &b in data {
        crc = (crc << 8) ^ table[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

pub struct OggOpusWriter<W: Write> {
    writer: W,
    serial: u32,
    page_seq: u32,
    granule: u64,
    sample_rate: u32,
    channels: u8,
    pre_skip: u16,
    samples_per_frame: u64,
    finished: bool,
}

impl<W: Write> OggOpusWriter<W> {
    pub fn new(writer: W, serial: u32, sample_rate: u32, channels: u8) -> io::Result<Self> {
        let pre_skip = 312; // 6.5ms at 48kHz, standard Opus encoder delay
        let mut s = Self {
            writer,
            serial,
            page_seq: 0,
            granule: 0,
            sample_rate,
            channels,
            pre_skip,
            samples_per_frame: (sample_rate as u64) / 50, // 20ms default
            finished: false,
        };
        s.write_id_header()?;
        s.write_comment_header()?;
        Ok(s)
    }

    fn write_id_header(&mut self) -> io::Result<()> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(self.channels);
        head.extend_from_slice(&self.pre_skip.to_le_bytes());
        head.extend_from_slice(&self.sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        self.write_page(&head, 0, 0x02)?; // BOS flag
        Ok(())
    }

    fn write_comment_header(&mut self) -> io::Result<()> {
        let vendor = b"Aurix 1.0.0";
        let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // comment count
        self.write_page(&tags, 0, 0x00)?;
        Ok(())
    }

    /// Write a single Opus packet as one or more Ogg pages.
    pub fn write_packet(&mut self, opus_data: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::new(io::ErrorKind::Other, "Writer already finished"));
        }
        self.granule += self.samples_per_frame;
        self.write_page(opus_data, self.granule, 0x00)
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // Write an empty EOS page
        self.write_page(&[], self.granule, 0x04) // EOS flag
    }

    fn write_page(&mut self, data: &[u8], granule: u64, header_type: u8) -> io::Result<()> {
        // Build segment table (each segment max 255 bytes; a 255-byte segment means continuation)
        let mut segments: Vec<u8> = Vec::new();
        let mut remaining = data.len();
        loop {
            if remaining >= 255 {
                segments.push(255);
                remaining -= 255;
            } else {
                segments.push(remaining as u8);
                break;
            }
        }
        if data.is_empty() {
            segments.push(0);
        }

        let num_segments = segments.len() as u8;
        let header_size = 27 + num_segments as usize;

        // Build header with CRC = 0 first, then compute CRC
        let mut page = Vec::with_capacity(header_size + data.len());
        page.extend_from_slice(OGG_CAPTURE);           // 0-3
        page.push(0);                                   // 4: version
        page.push(header_type);                          // 5: header type
        page.extend_from_slice(&granule.to_le_bytes());  // 6-13
        page.extend_from_slice(&self.serial.to_le_bytes()); // 14-17
        page.extend_from_slice(&self.page_seq.to_le_bytes()); // 18-21
        page.extend_from_slice(&0u32.to_le_bytes());     // 22-25: CRC placeholder
        page.push(num_segments);                         // 26
        page.extend_from_slice(&segments);               // segment table
        page.extend_from_slice(data);                    // payload

        let crc = ogg_crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());

        self.writer.write_all(&page)?;
        self.page_seq += 1;
        Ok(())
    }
}

impl<W: Write> Drop for OggOpusWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}