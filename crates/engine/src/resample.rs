// SPDX-License-Identifier: GPL-3.0-or-later
use crate::Error;
use rubato::{FftFixedIn, Resampler};

/// Bounded streaming conversion; remove the filter delay once and drain its tail at EOF.
pub(crate) struct Converter {
    resampler: FftFixedIn<f32>,
    input: [Vec<f32>; 2],
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
        Ok(Self {
            resampler,
            input: [Vec::new(), Vec::new()],
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
            let block = self
                .resampler
                .process(&[&self.input[0][..n], &self.input[1][..n]], None)
                .map_err(|e| Error::Decode(e.to_string()))?;
            for channel in &mut self.input {
                channel.drain(..n);
            }
            self.append(&block, &mut output);
        }
        Ok(output)
    }
    fn target(&self) -> u64 {
        (self.input_frames * self.output_rate as u64 + self.input_rate as u64 / 2)
            / self.input_rate as u64
    }
    fn append(&mut self, block: &[Vec<f32>], output: &mut Vec<[f32; 2]>) {
        let skip = self.skip.min(block[0].len());
        self.skip -= skip;
        let take =
            (self.target() - self.output_frames).min((block[0].len() - skip) as u64) as usize;
        output.extend((skip..skip + take).map(|i| [block[0][i], block[1][i]]));
        self.output_frames += take as u64;
    }
    pub fn remaining(&self) -> u64 {
        self.target().saturating_sub(self.output_frames)
    }
    pub fn finish(&mut self) -> Result<Vec<[f32; 2]>, Error> {
        let mut output = Vec::new();
        let input = if self.input[0].is_empty() {
            None
        } else {
            Some(self.input.as_slice())
        };
        let block = self
            .resampler
            .process_partial(input, None)
            .map_err(|e| Error::Decode(e.to_string()))?;
        self.append(&block, &mut output);
        while self.output_frames < self.target() {
            let block = self
                .resampler
                .process_partial::<Vec<f32>>(None, None)
                .map_err(|e| Error::Decode(e.to_string()))?;
            self.append(&block, &mut output);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
