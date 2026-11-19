//! SLIP framing for the client <-> server TCP link (`SLIP.hpp`).

pub const END: u8 = 0xC0;
pub const ESC: u8 = 0xDB;
pub const ESC_END: u8 = 0xDC;
pub const ESC_ESC: u8 = 0xDD;

/// Append `END <escaped frame> END` to `out`, like `SLIP::write_packet`.
pub fn encode(frame: &[u8], out: &mut Vec<u8>) {
    out.push(END);
    for &b in frame {
        match b {
            END => out.extend_from_slice(&[ESC, ESC_END]),
            ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
            _ => out.push(b),
        }
    }
    out.push(END);
}

/// Incremental SLIP decoder. Feed TCP chunks of any size; complete frames come out of the callback.
#[derive(Debug)]
pub struct Decoder {
    buf: Vec<u8>,
    max: usize,
    esc: bool,
    overflow: bool,
}

impl Decoder {
    /// Frames longer than `max_frame` decoded bytes are dropped whole, so memory stays bounded.
    /// The C++ client uses [`crate::MAX_PACKET_SIZE`] (`client.cpp:842`).
    pub fn new(max_frame: usize) -> Self {
        Self {
            buf: Vec::with_capacity(max_frame),
            max: max_frame,
            esc: false,
            overflow: false,
        }
    }

    /// Decode `chunk`, calling `on_frame` for every complete non-empty frame. Empty frames
    /// (back-to-back END) are skipped. As in the C++ reader, `ESC END` ends the frame and `ESC`
    /// followed by anything else yields that byte.
    pub fn feed(&mut self, chunk: &[u8], mut on_frame: impl FnMut(&[u8])) {
        for &b in chunk {
            let byte = if self.esc {
                self.esc = false;
                match b {
                    ESC_END => END,
                    ESC_ESC => ESC,
                    END => {
                        self.end_frame(&mut on_frame);
                        continue;
                    }
                    other => other,
                }
            } else {
                match b {
                    END => {
                        self.end_frame(&mut on_frame);
                        continue;
                    }
                    ESC => {
                        self.esc = true;
                        continue;
                    }
                    other => other,
                }
            };
            if self.buf.len() < self.max {
                self.buf.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }

    fn end_frame(&mut self, on_frame: &mut impl FnMut(&[u8])) {
        if !self.buf.is_empty() && !self.overflow {
            on_frame(&self.buf);
        }
        self.buf.clear();
        self.overflow = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(d: &mut Decoder, chunk: &[u8], frames: &mut Vec<Vec<u8>>) {
        d.feed(chunk, |f| frames.push(f.to_vec()));
    }

    #[test]
    fn golden() {
        let mut out = Vec::new();
        encode(&[1, END, 2, ESC, 3], &mut out);
        assert_eq!(out, [END, 1, ESC, ESC_END, 2, ESC, ESC_ESC, 3, END]);
    }

    #[test]
    fn split_at_every_byte() {
        let frames: [&[u8]; 4] = [&[1, 2, 3], &[END, ESC, END, ESC], &[ESC_END], &[0; 40]];
        let mut wire = vec![END, END]; // leading ENDs and an empty frame
        for f in frames {
            encode(f, &mut wire);
        }
        wire.extend_from_slice(&[END, END]);
        for i in 0..=wire.len() {
            for j in i..=wire.len() {
                let mut d = Decoder::new(64);
                let mut got = Vec::new();
                for chunk in [&wire[..i], &wire[i..j], &wire[j..]] {
                    collect(&mut d, chunk, &mut got);
                }
                assert_eq!(got, frames, "split at {i},{j}");
            }
        }
        // one byte at a time
        let mut d = Decoder::new(64);
        let mut got = Vec::new();
        for b in &wire {
            collect(&mut d, std::slice::from_ref(b), &mut got);
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn no_leading_end_and_oversize() {
        let mut d = Decoder::new(4);
        let mut got = Vec::new();
        // frame without leading END, then an oversize frame (dropped), then a max-size frame
        collect(
            &mut d,
            &[1, 2, END, 1, 2, 3, 4, 5, END, 9, 8, 7, ESC, ESC_ESC, END],
            &mut got,
        );
        assert_eq!(got, [vec![1, 2], vec![9, 8, 7, ESC]]);
        // hostile endless stream stays bounded
        let mut d = Decoder::new(16);
        for _ in 0..1000 {
            d.feed(&[0x55; 1024], |_| panic!("no frame expected"));
        }
        assert!(d.buf.capacity() <= 16);
    }

    #[test]
    fn odd_escapes() {
        let mut d = Decoder::new(16);
        let mut got = Vec::new();
        collect(&mut d, &[1, ESC, END, 2, ESC, 7, END], &mut got);
        assert_eq!(got, [vec![1], vec![2, 7]]);
    }
}
