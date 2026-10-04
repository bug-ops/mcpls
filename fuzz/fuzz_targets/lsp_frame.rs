#![no_main]

use std::io::Cursor;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use mcpls_core::lsp::LspTransport;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
});

fuzz_target!(|data: &[u8]| {
    RUNTIME.block_on(async {
        let (_writer, mut reader) =
            LspTransport::new(tokio::io::sink(), Cursor::new(data.to_vec()));
        while reader.receive().await.is_ok() {}
    });
});
