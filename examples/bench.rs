use mupdf::{Colorspace, Document, Matrix};
use std::time::Instant;
fn main() {
    for path in std::env::args().skip(1) {
        let t = Instant::now();
        let doc = Document::open(path.as_str()).unwrap();
        let n = doc.page_count().unwrap();
        let open = t.elapsed();
        let mut worst = 0f64; let mut total = 0f64;
        let pages: Vec<i32> = (0..n).step_by((n as usize / 10).max(1)).collect();
        for &p in &pages {
            let t = Instant::now();
            let page = doc.load_page(p).unwrap();
            let b = page.bounds().unwrap();
            let s = (1200.0 / (b.x1 - b.x0)).min(1920.0 / (b.y1 - b.y0));
            let _pix = page.to_pixmap(&Matrix::new_scale(s, s), &Colorspace::device_rgb(), false, false).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            worst = worst.max(ms); total += ms;
        }
        println!("{:>5.0}ms open, {:>3} pages, avg {:>5.1}ms worst {:>5.1}ms  {}", open.as_secs_f64()*1000.0, n, total / pages.len() as f64, worst, &path[path.len().saturating_sub(40)..]);
    }
}
