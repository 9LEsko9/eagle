// SPDX-License-Identifier: GPL-3.0-or-later
use crate::Error;
use rubato::{FftFixedIn, Resampler};

/// Bounded streaming conversion; remove the filter delay once and drain its tail at EOF.
pub(crate) struct Converter {
    resampler: FftFixedIn<f32>,
    input: [Vec<f32>; 2],
    block: Vec<Vec<f32>>,
    input_rate: u32,
    output_rate: u32,
    input_frames: u64,
    output_frames: u64,
    skip: usize,
}
impl Converter {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self, Error> {
        let resampler = FftFixedIn::new(input_rate as usize, output_rate as usize, 1024, 2, 2)
            .map_err(|e| Error::Decode(e.to_string()))?;
        let skip = resampler.output_delay();
        let block = resampler.output_buffer_allocate(true);
        Ok(Self {
            resampler,
            input: [Vec::new(), Vec::new()],
            block,
            input_rate,
            output_rate,
            input_frames: 0,
            output_frames: 0,
            skip,
        })
    }
    pub fn push(&mut self, frames: &[[f32; 2]]) -> Result<Vec<[f32; 2]>, Error> {
        self.input_frames += frames.len() as u64;
        for frame in frames {
            self.input[0].push(frame[0]);
            self.input[1].push(frame[1]);
        }
        let mut output = Vec::new();
        while self.input[0].len() >= self.resampler.input_frames_next() {
            let n = self.resampler.input_frames_next();
            let (_, written) = self
                .resampler
                .process_into_buffer(
                    &[&self.input[0][..n], &self.input[1][..n]],
                    &mut self.block,
                    None,
                )
                .map_err(|e| Error::Decode(e.to_string()))?;
            for channel in &mut self.input {
                channel.drain(..n);
            }
            self.append(written, &mut output);
        }
        Ok(output)
    }
    fn target(&self) -> u64 {
        (self.input_frames * self.output_rate as u64 + self.input_rate as u64 / 2)
            / self.input_rate as u64
    }
    fn append(&mut self, written: usize, output: &mut Vec<[f32; 2]>) {
        let skip = self.skip.min(written);
        self.skip -= skip;
        let take = (self.target() - self.output_frames).min((written - skip) as u64) as usize;
        output.extend((skip..skip + take).map(|i| [self.block[0][i], self.block[1][i]]));
        self.output_frames += take as u64;
    }
    pub fn remaining(&self) -> u64 {
        self.target().saturating_sub(self.output_frames)
    }
    pub fn finish(&mut self) -> Result<Vec<[f32; 2]>, Error> {
        let mut output = Vec::new();
        loop {
            let n = self.resampler.input_frames_next();
            for channel in &mut self.input {
                channel.resize(n, 0.);
            }
            let (_, written) = self
                .resampler
                .process_into_buffer(&self.input, &mut self.block, None)
                .map_err(|e| Error::Decode(e.to_string()))?;
            self.append(written, &mut output);
            for channel in &mut self.input {
                channel.clear();
            }
            if self.output_frames >= self.target() {
                break;
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_output_matches_allocating_resampler_through_partial_tail() {
        for (from, to) in [(8_000, 44_100), (44_100, 48_000), (48_000, 44_100)] {
            for len in [1, 127, 1024, from + 13] {
                let input: Vec<_> = (0..len)
                    .map(|i| [(i as f32 * 0.04).sin(), (i as f32 * 0.07).cos()])
                    .collect();
                let target = (len as u64 * to as u64 + from as u64 / 2) / from as u64;
                let mut reference = FftFixedIn::<f32>::new(from, to, 1024, 2, 2).unwrap();
                let delay = reference.output_delay();
                let mut expected = Vec::new();
                let mut offset = 0;
                while expected.len() < target as usize + delay {
                    let n = reference.input_frames_next();
                    let mut padded = [vec![0.; n], vec![0.; n]];
                    for (index, frame) in input.iter().skip(offset).take(n).enumerate() {
                        padded[0][index] = frame[0];
                        padded[1][index] = frame[1];
                    }
                    offset = (offset + n).min(input.len());
                    let block = reference.process(&padded, None).unwrap();
                    expected.extend(block[0].iter().zip(&block[1]).map(|(&l, &r)| [l, r]));
                }
                let expected = &expected[delay..delay + target as usize];
                let mut converter = Converter::new(from as u32, to as u32).unwrap();
                let addresses = [converter.block[0].as_ptr(), converter.block[1].as_ptr()];
                let mut actual = Vec::new();
                for chunk in input.chunks(331) {
                    for channel in &mut converter.block {
                        channel.fill(f32::NAN);
                    }
                    actual.extend(converter.push(chunk).unwrap());
                }
                actual.extend(converter.finish().unwrap());
                assert_eq!(actual, expected);
                assert_eq!(converter.block[0].as_ptr(), addresses[0]);
                assert_eq!(converter.block[1].as_ptr(), addresses[1]);
            }
        }
    }
    #[test]
    fn trims_delay_and_tail_and_preserves_frame_count_in_both_directions() {
        for (from, to) in [(44_100, 48_000), (48_000, 44_100)] {
            for len in [1, 107, 1024, 4437, from as usize] {
                let mut converter = Converter::new(from, to).unwrap();
                let input: Vec<_> = (0..len).map(|i| [(i as f32 * 0.04).sin(); 2]).collect();
                let mut frames = Vec::new();
                for chunk in input.chunks(331) {
                    frames.extend(converter.push(chunk).unwrap());
                }
                frames.extend(converter.finish().unwrap());
                assert_eq!(
                    frames.len() as u64,
                    (len as u64 * to as u64 + from as u64 / 2) / from as u64
                );
                assert!(frames.iter().flatten().all(|x| x.is_finite()));
                if len > 4000 {
                    assert!(frames[1000..2000].iter().any(|s| s[0].abs() > 0.5));
                }
            }
        }
    }
}
