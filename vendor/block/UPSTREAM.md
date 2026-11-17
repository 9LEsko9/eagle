# block 0.1.6

Source: [rust-block](https://github.com/SSheldon/rust-block), author Steven Sheldon,
MIT license as declared in the original Cargo.toml. Crates.io archive SHA-256:
`0d8c1fef690941d3e7788d328517591fecc684c084084702d6ff1641e993699a`.

This compatibility patch declares the opaque `_NSConcreteStackBlock` symbol as
an inhabited byte and takes its raw address without creating a Rust reference
to the foreign class. Block layout and the public API are unchanged. Existing
implicit extern ABIs are spelled explicitly as `extern "C"`, with the same ABI.
This resolves Rust's `uninhabited_static` warning without migrating souvlaki's
Objective-C bindings to the unrelated block2 API. The native capture, copy,
invocation and release path is checked in `crates/ui/tests/block_abi.rs`.

Версия остаётся 0.1.6: более новой опубликованной версии нет. Локальный patch
исправляет объявление непрозрачного runtime-символа: используется байт и его
сырой адрес, без Rust-ссылки на необитаемый тип. ABI и публичный API сохраняются;
прежний неявный extern ABI явно записан как C. Тест macOS проверяет захват
данных, копирование, вызов и освобождение блока.
