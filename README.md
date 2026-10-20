# sheetview

A fast, furniture-free PDF viewer for playing sheet music from a touch-screen laptop or tablet.

- **Library:** a grid of first-page thumbnails for every PDF in a folder. Tap one to open it.
- **Viewer:** the page fills the screen with no toolbars or menus.
  - Tap the **right half** for the next page and the **left half** for the previous one. The page turns as soon as your finger touches the screen.
  - **Swipe left or right**, or tap the **top-centre strip**, to go back to the library.
- **Fast page turns:** pages are rendered with [MuPDF](https://mupdf.com/) at the screen's exact pixel size, and the next pages render in the background while you play.
- **Remembers your place** in each piece.
- **Stays fullscreen**, even if a stray gesture knocks it out. It re-renders when you rotate the screen.

## Keys

| Key | Action |
| --- | --- |
| → ↓ PgDn Space Enter | Next page |
| ← ↑ PgUp Backspace | Previous page |
| Home / End | First / last page |
| Esc, Back key, mouse back button | Back to library (Esc or Q in the library quits) |

Page-turner pedals that send arrow or PgDn keys work too.

## Usage

```sh
sheetview ~/Documents/sheetmusic   # open the library for a folder
sheetview song.pdf                 # open one PDF directly
```

With no argument, sheetview shows the current directory. Thumbnails and reading positions are kept in `~/.cache/sheetview/`.

## Building

Requires Rust, a C compiler and libclang. The `mupdf` crate builds MuPDF from source, so the first build takes a few minutes.

```sh
cargo build --release
cp target/release/sheetview ~/.local/bin/
```

If bindgen can't find libclang or reports `'stddef.h' file not found`, point it at your system's copies in `.cargo/config.toml` (adjust the paths for your system):

```toml
[env]
LIBCLANG_PATH = "/usr/lib/llvm-21/lib"
BINDGEN_EXTRA_CLANG_ARGS = "-I/usr/lib/gcc/x86_64-linux-gnu/15/include"
```

`cargo run --release --example bench -- file.pdf` reports how long a file takes to render per page.

## License

MIT. Note that MuPDF, which is compiled into the binary, is licensed under the AGPL-3.0, so binaries you distribute fall under the AGPL's terms.
