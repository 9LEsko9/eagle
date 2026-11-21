# femtovg 0.25.1

Source: [upstream](https://github.com/femtovg/femtovg/tree/f32dc79cd4762feb6027c55d2bf10d39de3c7767),
crates.io archive SHA-256 `f43d05da42e81724c16d34a150fb7fda53d3f786e5a94673ee526ff8602f0f6a`.
The original MIT and Apache-2.0 licenses are included.

Eagle pins this copy because Slint 1.17 uses WGPU 29. The newer femtovg memory
work uses WGPU 30 and cannot be substituted directly. Only
`src/renderer/wgpu.rs` carries the renderer changes below; `src/lib.rs` also gates
the file-path import to the features that use it, avoiding an upstream unused
import warning when rendering without image-loading/textlayout:

- On macOS, submit and wait after batches of eight render passes to bound retained
  Metal encoder resources without synchronizing after every individual pass.
- Start passes only when needed, while preserving clear-only passes.
- Keep recently used macOS pipelines for up to 64 flushes; cap the cache at 512 entries.
- Log the actual backend when creating a device-backed renderer. The isolated
  desktop benchmark uses this instead of installing Slint's rendering notifier,
  whose additional flush changes pipeline caching.
- `EAGLE_PROFILE_FRAME_TIME=1` records command encoding durations for separate
  diagnostic runs; these are not presentation intervals.

Earlier slices are submitted during `flush_to_output`, so callers must submit
prerequisite GPU work before flushing, and submit the returned command buffer
before the next flush. Eagle's Slint integration follows this ordering.

## По-русски

Это закреплённая копия femtovg 0.25.1 с исходными лицензиями MIT/Apache-2.0.
Изменён только WGPU backend: на macOS GPU-пакеты ограничены восемью render pass
с ожиданием завершения между пакетами, а не после каждого pass.
Пустые pass не создаются, а очистки сохраняются.
Кэш pipeline ограничен 512 записями и удерживает неиспользуемые записи не более
64 flush. Логируется фактический backend; дополнительный rendering notifier
Slint для этого не нужен. Диагностический флаг выше измеряет кодирование команд,
а не показ кадров. Предшествующую GPU-работу нужно отправлять до flush, а
возвращённый command buffer — до следующего flush. Для прямого обновления на
новый upstream потребуется совместимая версия Slint/WGPU.
