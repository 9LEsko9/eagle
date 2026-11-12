// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs::File;
use symphonia::core::{
    formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for file in std::env::args().skip(1) {
        let stream = MediaSourceStream::new(Box::new(File::open(&file)?), Default::default());
        let probe = symphonia::default::get_probe().format(
            &Hint::new(),
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )?;
        for track in probe.format.tracks() {
            let p = &track.codec_params;
            let codec = symphonia::default::get_codecs()
                .get_codec(p.codec)
                .map_or("unknown", |c| c.short_name);
            println!(
                "{} codec={} rate={:?} channels={:?} frames={:?} delay={:?} padding={:?}",
                std::path::Path::new(&file)
                    .file_name()
                    .unwrap_or_default()
                    .display(),
                codec,
                p.sample_rate,
                p.channels.map(|c| c.count()),
                p.n_frames,
                p.delay,
                p.padding
            );
        }
    }
    Ok(())
}
