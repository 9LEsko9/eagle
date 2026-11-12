// SPDX-License-Identifier: GPL-3.0-or-later
//! Emits only public identifiers and transport facts, never signed URLs or tokens.
use eagle_core::{Provider, Quality};
use eagle_net::{Client, Priority, Request};
use std::sync::Arc;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let net = Arc::new(Client::new()?);
    let provider = eagle_yandex::Yandex::new(net.clone());
    for id in std::env::args().skip(1) {
        let track = id.parse()?;
        for quality in [Quality::High, Quality::Standard] {
            let stream = provider.resolve_stream(&track, quality)?;
            let mut request = Request::get(stream.url);
            request.range = Some((0, 65535));
            request.limit = 65536;
            let response = net.request(Priority::Playback, request, None)?;
            let magic = if response.bytes.starts_with(b"ID3")
                || response
                    .bytes
                    .windows(2)
                    .take(4096)
                    .any(|b| b[0] == 255 && b[1] & 0xe0 == 0xe0)
            {
                "mpeg-audio"
            } else {
                "unidentified"
            };
            println!(
                "{}",
                serde_json::json!({"provider":"yandex","id":id,"quality":quality.key(),"declared_codec":stream.codec,"container_probe":magic,"status":response.status,"content_range":response.content_range,"etag_present":response.etag.is_some(),"sample_bytes":response.bytes.len(),"encryption":format!("{:?}",stream.encryption)})
            );
        }
    }
    Ok(())
}
