// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    if std::env::args().any(|arg| arg == "--bench-idle") {
        loop {
            std::thread::park();
        }
    }
}
