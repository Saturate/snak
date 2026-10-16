//! make-icon <icon.svg> <out.iconset>: every size macOS's iconutil expects.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(svg), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: make-icon <icon.svg> <out.iconset>".into());
    };
    let svg = std::fs::read(svg)?;
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())?;
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    for base in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let px = base * scale;
            let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px).ok_or("bad size")?;
            let factor = px as f32 / tree.size().width();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(factor, factor),
                &mut pixmap.as_mut(),
            );
            let name = if scale == 1 {
                format!("icon_{base}x{base}.png")
            } else {
                format!("icon_{base}x{base}@2x.png")
            };
            pixmap.save_png(out.join(name))?;
        }
    }
    Ok(())
}
