# i-slint-renderer-femtovg 1.17.0

Source: [Slint](https://github.com/slint-ui/slint/tree/fdde7a535305d2ab2d4072dee637bad186a49723/internal/renderers/femtovg),
crates.io archive SHA-256 `c1a430c5adbd652119a5d37673b424ce3516903e192d73848919153e503c6d4d`.
The upstream license alternatives and their texts are preserved in `LICENSES`.

The only code change is in `wgpu.rs`: macOS surfaces request maximum frame latency
1 before configuration. WGPU's Metal backend allocates latency + 1 drawables, so
this uses two full-size surfaces instead of three. Applying the limit after
configuration allows a temporary third surface to inflate the lifetime peak.
The setting survives resize and surface recreation through the normal WGPU API.

## По-русски

Исходный Slint 1.17.0 с сохранёнными лицензиями. Единственное изменение кода —
`desired_maximum_frame_latency = 1` на macOS до настройки поверхности. WGPU/Metal
использует latency + 1 поверхностей: две вместо трёх. Позднее изменение лимита
не предотвращает временный пик третьей поверхности. Настройка применяется
штатным API WGPU и сохраняется при изменении размера и пересоздании поверхности.
