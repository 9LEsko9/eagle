# i-slint-renderer-femtovg 1.17.0

Source: [Slint](https://github.com/slint-ui/slint/tree/fdde7a535305d2ab2d4072dee637bad186a49723/internal/renderers/femtovg),
crates.io archive SHA-256 `c1a430c5adbd652119a5d37673b424ce3516903e192d73848919153e503c6d4d`.
The upstream license alternatives and their texts are preserved in `LICENSES`.

In `wgpu.rs`, macOS surfaces explicitly retain WGPU's
default maximum frame latency of 2 before configuration. WGPU's Metal backend
allocates latency + 1 drawables. Three drawables let CPU and GPU work overlap;
reducing this to two saves memory at the cost of rendering throughput.
The setting survives resize and surface recreation through the normal WGPU API.

In `itemrenderer.rs`, fully transparent subtrees skip drawing and release their
cached layer. This prevents hidden controls in virtualized lists from allocating
offscreen textures and issuing GPU passes when rows are created during scrolling.
Positive opacity still follows the upstream compositing path, including fades.

## По-русски

Исходный Slint 1.17.0 с сохранёнными лицензиями. В `wgpu.rs` явно задано
`desired_maximum_frame_latency = 2` на macOS до настройки поверхности —
стандартное значение WGPU. WGPU/Metal использует latency + 1 поверхностей:
три позволяют CPU и GPU работать параллельно; сокращение до двух экономит память
за счёт пропускной способности рендеринга. Настройка применяется
штатным API WGPU и сохраняется при изменении размера и пересоздании поверхности.

В `itemrenderer.rs` полностью прозрачные поддеревья пропускают отрисовку и
освобождают свой кэшированный слой. Скрытые кнопки виртуального списка больше
не создают текстуры и GPU-проходы при появлении строк во время прокрутки.
При положительной прозрачности, включая плавные переходы, используется исходный
путь композитинга.
