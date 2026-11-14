# Public track fixture

`public-track.json` is a reduced result from the public
`https://api-v2.soundcloud.com/search/tracks` endpoint recorded on 2026-11-14.
Unconsumed fields were removed; track and uploader IDs, names, and transcoding
URLs were replaced with fictional values. No client identifier or signed
playback URL is retained. The search envelope, client discovery, retry, and
stream response in the conformance test are simulated.
